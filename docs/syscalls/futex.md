---
schema: syscall-pages/v1
syscall: futex
nr_x86_64: 202
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex&body=Discussion+for+the+%60futex%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 63
---

## Summary

`futex(uaddr, futex_op, val, timeout_or_val2, uaddr2, val3)` is the kernel primitive under every userspace mutex, condition variable, semaphore and thread join. One syscall carries many operations, chosen by `futex_op`. Detcore handles it (`detcore/src/lib.rs:2429`, `detcore/src/syscalls/threads.rs:1223`). When threads are sequentialized, Detcore models `FUTEX_WAIT`, `FUTEX_WAKE`, `FUTEX_WAIT_BITSET` and `FUTEX_WAKE_BITSET` with a wait queue inside its own scheduler and never sends them to the kernel. Every other operation stops the run with a `panic!` (`detcore/src/syscalls/threads.rs:1367`, `detcore/src/syscalls/threads.rs:1370`).

Manual reference: Linux man-pages futex(2); constants from `<linux/futex.h>`.

## Linux semantics

`uaddr` points to a 32-bit futex word, which must be 4-byte aligned. The low bits of `futex_op` select the command (`FUTEX_CMD_MASK`). Two modifier bits can be added: `FUTEX_PRIVATE_FLAG` (128) and `FUTEX_CLOCK_REALTIME` (256). The fourth argument is a `struct timespec *` for the commands that can wait: `FUTEX_WAIT`, `FUTEX_WAIT_BITSET`, `FUTEX_LOCK_PI`, `FUTEX_LOCK_PI2` and `FUTEX_WAIT_REQUEUE_PI`. For `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE`, `FUTEX_WAKE_OP` and `FUTEX_CMP_REQUEUE_PI` the same argument is an integer `val2`.

A wait is a compare-and-block. The kernel takes the hash-bucket lock, reads `*uaddr` and compares it with `val`. If they differ, it returns `EAGAIN` at once. Otherwise it queues the caller before releasing the lock. A wake that runs after the waiter's store to the futex word therefore cannot be lost.

Each waiter is keyed on the futex word:

- A private futex (`FUTEX_PRIVATE_FLAG`) is keyed on the (mm, address) pair. Only threads sharing that address space can meet on it.
- A shared futex is keyed on the backing object and offset: an inode, or the shared anonymous mapping. Processes that map the same page at different addresses still meet.

The kernel keeps waiters in a priority list per hash bucket. Non-realtime waiters of equal priority are woken in FIFO order, and realtime-priority waiters go first. Which waiter actually runs next, and when, is up to the host scheduler.

Check order in the kernel:

1. `FUTEX_CLOCK_REALTIME` with a command that does not accept it gives `ENOSYS`.
2. An unknown command gives `ENOSYS`.
3. A timeout argument that is a bad pointer gives `EFAULT`; an invalid timespec gives `EINVAL`.
4. Then the per-command checks run: alignment, `bitset == 0`, `uaddr == uaddr2` for the requeue-PI pair, and so on.

### Flags and options

- F1: `FUTEX_WAIT` (0): if `*uaddr == val`, sleep until woken, until the timeout expires (`ETIMEDOUT`), or until a signal arrives. Returns 0 when woken. Equivalent to `FUTEX_WAIT_BITSET` with `val3 = FUTEX_BITSET_MATCH_ANY`. Detcore models it in its scheduler queue (`detcore/src/syscalls/threads.rs:1315`).
- F2: `FUTEX_WAKE` (1): wake up to `val` waiters on `uaddr` and return how many were woken. `val` is usually 1 or `INT_MAX`. Detcore models it (`detcore/src/syscalls/threads.rs:1277`) and returns the count from its own queue (`detcore/src/syscalls/threads.rs:1313`).
- F3: `FUTEX_FD` (2): removed in Linux 2.6.26; the kernel returns `ENOSYS`. Detcore panics with "refusing to execute FUTEX_FD" (`detcore/src/syscalls/threads.rs:1367`).
- F4: `FUTEX_REQUEUE` (3): wake up to `val` waiters on `uaddr`, then move up to `val2` of the rest to wait on `uaddr2` without waking them. No compare step. Detcore panics with "futex op not handled yet" (`detcore/src/syscalls/threads.rs:1370`).
- F5: `FUTEX_CMP_REQUEUE` (4): like F4, but first checks `*uaddr == val3` and returns `EAGAIN` if it differs. The man page says it returns the total woken plus requeued. musl's condition variable requeues waiters with it. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F6: `FUTEX_WAKE_OP` (5): atomically read `*uaddr2` into `oldval` and store the result of an operation. Then wake up to `val` waiters on `uaddr`. If the comparison of `oldval` against `cmparg` holds, also wake up to `val2` waiters on `uaddr2`. Returns the total woken. `val3` encodes the operation as `FUTEX_OP(op, oparg, cmp, cmparg)`: 4 bits of `op`, 4 bits of `cmp`, 12-bit `oparg` and 12-bit `cmparg`.
  - `op` is one of `FUTEX_OP_SET`, `FUTEX_OP_ADD`, `FUTEX_OP_OR`, `FUTEX_OP_ANDN` or `FUTEX_OP_XOR`. Or-ing in `FUTEX_OP_OPARG_SHIFT` (8) uses `1 << oparg` instead of `oparg`.
  - `cmp` is one of `FUTEX_OP_CMP_EQ`, `_NE`, `_LT`, `_LE`, `_GT` or `_GE`.
  - An undefined `op` or `cmp` gives `ENOSYS`.

  Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F7: `FUTEX_LOCK_PI` (6): take a priority-inheritance lock. The futex word holds the owner's TID, plus `FUTEX_WAITERS` and `FUTEX_OWNER_DIED` bits. The kernel boosts the owner while the caller waits. The timeout is absolute against `CLOCK_REALTIME`. The call can fail with `EDEADLK` when the caller already owns the lock, and with `ESRCH` when the TID in the word does not exist. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F8: `FUTEX_LOCK_PI2` (13, Linux 5.14): as F7, but the absolute timeout is measured on `CLOCK_MONOTONIC` unless `FUTEX_CLOCK_REALTIME` is set. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F9: `FUTEX_UNLOCK_PI` (7): release a PI lock and hand it to the highest-priority waiter. The kernel returns `EPERM` if the caller is not the owner. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F10: `FUTEX_TRYLOCK_PI` (8): try once to take a PI lock without waiting. It also repairs stale owner state. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F11: `FUTEX_WAIT_BITSET` (9): as F1, but the waiter records the bitset `val3`. `val3 == 0` gives `EINVAL`. The timeout is an absolute deadline: on `CLOCK_MONOTONIC` by default, or on `CLOCK_REALTIME` with F16. glibc's condition variables and timed locks use it. Detcore models it, with the bitset stored on the queued waiter (`detcore/src/syscalls/threads.rs:1268`).
- F12: `FUTEX_WAKE_BITSET` (10): as F2, but it wakes only waiters whose bitset intersects `val3`. `val3 == 0` gives `EINVAL`. Detcore models it (`detcore/src/syscalls/threads.rs:1277`; matching at `detcore/src/scheduler.rs:240`) and returns `EINVAL` for a zero bitset (`detcore/src/syscalls/threads.rs:1272`).
- F13: `FUTEX_WAIT_REQUEUE_PI` (11): wait on the non-PI futex `uaddr` and expect a later F14 to move the caller onto the PI futex `uaddr2`. It returns owning `uaddr2`. The timeout is absolute. `uaddr == uaddr2` gives `EINVAL`. This is glibc's PI condition variable path. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F14: `FUTEX_CMP_REQUEUE_PI` (12): the waking half of F13. Compares `*uaddr` with `val3`, wakes at most one waiter (`val` must be 1), and requeues up to `val2` waiters onto the PI futex `uaddr2`. Detcore panics (`detcore/src/syscalls/threads.rs:1370`).
- F15: `FUTEX_PRIVATE_FLAG` (128): key the futex on (mm, address) instead of the shared mapping. The flag is valid with every command. Detcore honours it in `futex_id` (`detcore/src/syscalls/threads.rs:1263`):
  - Private keys are (mm, address) (`detcore/src/tool_local.rs:2439`).
  - Shared keys are (mapping object, offset) when the address lies in a tracked shared mapping (`detcore/src/memory.rs:100`). Otherwise they fall back to a private key.
- F16: `FUTEX_CLOCK_REALTIME` (256): measure the timeout on `CLOCK_REALTIME`. It is accepted with `FUTEX_WAIT` (Linux 4.5), `FUTEX_WAIT_BITSET`, `FUTEX_WAIT_REQUEUE_PI` and `FUTEX_LOCK_PI2`. With any other command the kernel returns `ENOSYS`. Detcore reads the flag only when it has to sample a clock for an absolute deadline (`detcore/src/syscalls/threads.rs:804`). Otherwise it masks the flag off with `FUTEX_CMD_MASK` (`detcore/src/syscalls/threads.rs:1267`), so `FUTEX_WAKE|FUTEX_CLOCK_REALTIME` is served as a plain wake instead of `ENOSYS`.
- F17: timeouts.
  - `FUTEX_WAIT` takes a relative interval. `FUTEX_WAIT_BITSET`, `FUTEX_WAIT_REQUEUE_PI`, `FUTEX_LOCK_PI` and `FUTEX_LOCK_PI2` take an absolute deadline. A null timeout waits forever.
  - Expiry gives `ETIMEDOUT`.
  - A negative `tv_sec` or `tv_nsec`, or `tv_nsec >= 1e9`, gives `EINVAL`.
  - Detcore parses the timeout in `parse_futex_timeout` (`detcore/src/syscalls/threads.rs:739`), which returns `EINVAL` for invalid or overflowing values (`detcore/src/syscalls/threads.rs:740`). It treats the timeout as absolute only for `FUTEX_WAIT_BITSET` after masking (`detcore/src/syscalls/threads.rs:756`).
  - Detcore then turns it into a logical-time deadline in `futex_timeout_deadline` (`detcore/src/syscalls/threads.rs:783`). The scheduler wakes the waiter with `ETIMEDOUT` when logical time reaches that deadline (`detcore/src/syscalls/threads.rs:1358`).
- F18: value mismatch: a wait whose `*uaddr != val` returns `EAGAIN` (also spelled `EWOULDBLOCK`) without sleeping. Detcore compares `val` with the word it read at handler entry (`detcore/src/syscalls/threads.rs:1236`) and returns `EAGAIN` without queuing (`detcore/src/syscalls/threads.rs:1316`).
- F19: signals and spurious wakeups.
  - A wait interrupted by a signal handler returns `EINTR`. Since Linux 2.6.22 `FUTEX_WAIT` is restarted instead when the handler has `SA_RESTART`.
  - The kernel no longer reports spurious wakeups as `EINTR`. A caller can still return 0 without having been the intended target, for example after a wake meant for an earlier waiter, so callers must re-check the futex word.
  - In Detcore, a parked waiter returns only on a wake, a timeout, or teardown of a killed thread (`detcore/src/tool_global.rs:2440`). Teardown answers `EINTR`, but `handle_futex_blocking` maps every non-timeout answer to 0 (`detcore/src/syscalls/threads.rs:1336`).
  - This body found no path that delivers a signal to a parked futex waiter. The cross-task signal drain at `detcore/src/scheduler.rs:5263` re-queues waitid-parked threads and restartable internal-I/O waiters, not futex waiters.
- F20: invalid operations.
  - An undefined command number (14 and above, or a garbage value) gives `ENOSYS`.
  - `FUTEX_CLOCK_REALTIME` on a command that cannot wait gives `ENOSYS` (F16).
  - A misaligned `uaddr` gives `EINVAL`, as does a zero bitset.

  Detcore panics on every unknown command (`detcore/src/syscalls/threads.rs:1370`), so an unknown op ends the run instead of returning `ENOSYS`.
- F21: which waiters a wake picks, and how many.
  - Linux wakes in priority order, FIFO within equal priority. Its wake loop counts a waiter before comparing the count with `val`, so by reading of the kernel's `futex_wake` loop, `val <= 0` still wakes one waiter. That is a hypothesis for T12.
  - Detcore keeps waiters in a `Vec` per key (`detcore/src/scheduler.rs:261`) and takes them from the end with `split_off` (`detcore/src/scheduler.rs:2809`), so the most recently queued waiters wake first. With `--fuzz-futexes` it first applies a seeded partial shuffle (`detcore/src/scheduler.rs:2791`).
  - Detcore returns 0 for `val == 0` (`detcore/src/scheduler.rs:2818`).
  - A negative `val` reaches `max_to_wake.try_into().unwrap()` when waiters are present (`detcore/src/scheduler.rs:2840`), which would panic.
- F22: futex-word address.
  - On Linux, a null or unmapped `uaddr` gives `EFAULT` for waits. A private `FUTEX_WAKE` never reads the word, so it returns 0 even on an unmapped address. A shared `FUTEX_WAKE` must resolve the page, so it gives `EFAULT`.
  - Detcore sends a null `uaddr` to the kernel unchanged (`detcore/src/syscalls/threads.rs:1229`).
  - For any other address, Detcore reads the word before dispatch (`detcore/src/syscalls/threads.rs:1236`), for every command. By reading of the code, a private wake on an unmapped address therefore returns `EFAULT` where Linux returns 0. That is a hypothesis for T9.
  - Detcore performs no alignment check of its own.
- F23: commands Hermit does not model (owner decision 10 interim): every command other than F1, F2, F11 and F12, after `FUTEX_CMD_MASK`, returns `ENOSYS`, the answer Linux gives for a command it does not implement (F3, F20). It does so without reading the futex word, as Linux decides the command before touching `uaddr`, so an unmapped `uaddr` still gets `ENOSYS`. The guest continues and the tracer does not stop. F4 to F6 leave this list when modelled (F24); the PI family (F7 to F10, F13, F14) stays on it until modelled.
- F24: the requeue and wake-op model, the first commands to model (owner decision 10): `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` and `FUTEX_WAKE_OP` with Linux's results (F4 to F6) and Linux's argument checks: a negative `val` or `val2` gives `EINVAL` for the two requeue commands (`futex_requeue`, `kernel/futex/requeue.c`), the compare of F5 happens with both words resolved and gives `EAGAIN`, an undefined `op` or `cmp` in `FUTEX_WAKE_OP` gives `ENOSYS`, and an unmapped or misaligned `uaddr2` gives `EFAULT` or `EINVAL`. Waiters moved by a requeue keep their place in Detcore's queue order and wake as waiters of `uaddr2`.

## Determinism analysis

`linux_determinism: nondeterministic`. This replaces the migration placeholder `conditional`, which had no legacy classification behind it. The arguments are deterministic, but the outcome of every blocking or waking futex call depends on the host:

- which waiter a wake picks when several sleep with equal priority;
- whether the waker's store and wake land before or after the waiter's compare, which decides between `EAGAIN` (F18) and sleeping;
- when a timeout fires relative to a wake (F17);
- when a signal interrupts a wait (F19).

Even a single-threaded program gets a host-timing-dependent `ETIMEDOUT` duration from a timed wait. Only an uncontended call with no timeout, such as a wake with no waiters or a wait whose value differs, repeats on its own.

### Nondeterminism sources

- Host thread scheduling, which orders the compare, the store and the wake across threads (F1, F2, F11, F12, F18)
- The kernel's choice of waiter among equals, and realtime priorities (F21; PI boosting for F7 to F10)
- Host clocks behind relative and absolute timeouts (F16, F17)
- Signal arrival time (F19)
- For shared futexes, processes outside the guest that map the same file (F15)
- PI-futex owner-death and TID state, which depends on host TIDs that Hermit virtualizes (F7 to F10)

## Hermit design

Target policy: `model`. Under `sequentialize_threads`, Detcore already owns the futex wait queue for the commands it supports. Its doc comment states the rule: "We EMULATE futex calls and NEVER run them inside the kernel" (`detcore/src/syscalls/threads.rs:1253`). `hermit run` sequentializes by default: `config.sequentialize_threads = self.strict || !self.no_sequentialize_threads` (`hermit-cli/src/bin/hermit/run.rs:3667`). The rest of this section is about that mode, with the default `--debug-futex-mode precise` (`detcore-model/src/config.rs:589`).

What the model does today:

- **Dispatch.** `handle_futex` reads the word (`detcore/src/syscalls/threads.rs:1236`). Without sequentialization, it sends the call to the kernel unchanged (`detcore/src/syscalls/threads.rs:1242`). Otherwise it picks the Precise, Polling or External implementation (`detcore/src/syscalls/threads.rs:1246`).
- **Wait (F1, F11).** On a value mismatch the wait returns `EAGAIN` (F18). Otherwise Detcore sends `FutexAction::WaitRequest(deadline)` to the scheduler (`detcore/src/syscalls/threads.rs:1328`). The scheduler queues the waiter under its `FutexID` and bitset (`detcore/src/tool_global.rs:2478`, `sleep_futex_waiter` at `detcore/src/scheduler.rs:2724`) and takes the thread off the run queue (`detcore/src/tool_global.rs:2493`). A deadline also enters `timed_waiters`, so logical time can wake it (`detcore/src/scheduler.rs:2756`).
  - Because only the thread holding the scheduler turn runs guest code, the compare and the enqueue are not separated by other guest instructions, which removes the lost-wakeup race.
  - A wait on a child-TID futex that has already been cleared returns 0 at once (`detcore/src/tool_global.rs:2460`). `CLONE_CHILD_CLEARTID` itself is simulated by `wake_futex_child_cleartid` (`detcore/src/scheduler.rs:2896`).
- **Wake (F2, F12).** `FutexAction::WakeRequest(val)` goes to `wake_futex_waiters` (`detcore/src/tool_global.rs:2484`). That function filters waiters by bitset (`detcore/src/scheduler.rs:240`), chooses the waiters deterministically (`detcore/src/scheduler.rs:2786`) and pushes them onto the run queue (`detcore/src/scheduler.rs:2762`). The guest sees the exact number woken.
- **Timeouts (F16, F17).**
  - Relative timeouts are added to the thread's observed logical time (`detcore/src/syscalls/threads.rs:794`).
  - With `virtualize_time`, an absolute `FUTEX_WAIT_BITSET` deadline is used directly as a logical time (`detcore/src/syscalls/threads.rs:798`). This works because the guest computes its deadline from a virtual clock. `handle_clock_gettime` returns the same virtual time for every clock id (`detcore/src/syscalls/time.rs:204`), so `CLOCK_REALTIME` and `CLOCK_MONOTONIC` deadlines share one domain.
  - For SaBRe, whose direct vDSO clock reads bypass virtualization, `hermit-cli/src/lib.rs:2452` sets `detect_host_clock_futex_timeouts`. Detcore then reads the live host clock and rebases host-domain deadlines onto logical time (`detcore/src/syscalls/threads.rs:816`, `detcore/src/syscalls/threads.rs:837`).
  - Commit 5262fbf9b22e, from https://github.com/rrnewton/hermit/pull/233, made a private-flagged `FUTEX_WAIT_BITSET` count as absolute (`detcore/src/syscalls/threads.rs:750`).
- **Keys (F15).** Private and shared keys are derived as described under F15. A MAP_SHARED anonymous futex wakes across `fork` (test `shared_anonymous_futex_wakes_across_processes`, `detcore/tests/misc/mod.rs:1630`).
- **Teardown.** A thread killed while parked is removed from every wait pool (`detcore/src/scheduler.rs:2299`, `detcore/src/scheduler.rs:2694`). Robust-list futexes of an exiting thread are woken (`detcore/src/scheduler.rs:2871`).

The alternative modes are for debugging:

- `polling` probes `FUTEX_WAIT` in the kernel with a zero timeout (`detcore/src/syscalls/helpers.rs:1122`) and sends wakes to the kernel. Its own FIXME says the woken count is wrong (`detcore/src/syscalls/threads.rs:1404`).
- `external` records or replays the blocking call (`detcore/src/syscalls/threads.rs:1248`).

Why `model` and not the alternatives:

- `passthrough` would let a thread sleep in the kernel where the deterministic scheduler cannot see it. That is the deadlock or nondeterminism that sequentialization exists to prevent, and it is exactly what `--no-sequentialize-threads` trades away.
- `emulate` with fixed answers cannot provide mutual exclusion.
- `refuse` would break every threaded program.

What remains before the code fully realizes `model`:

- **Unhandled commands panic.** F4 to F10, F13, F14 and every unknown command (F20) reach `panic!("[detcore] futex op not handled yet: {}", other)` in both the precise and polling paths (`detcore/src/syscalls/threads.rs:1370`, `detcore/src/syscalls/threads.rs:1437`).
  - `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` and `FUTEX_WAKE_OP` fit the existing queue: move waiters between keys, and do a guest-memory read-modify-write while the turn is held.
  - The PI family also needs the TID-in-word protocol, with virtual TIDs. Priority inheritance can be modelled as a no-op, because the scheduler has no priorities.
  - Until each command is modelled, the interim answer is `ENOSYS` (owner decision 10; see the target below). A Rust panic is not an answer. `ENOSYS` is also Linux's own errno for an undefined command and for F3.
- **FUTEX_FD (F3)** panics where Linux returns `ENOSYS` (`detcore/src/syscalls/threads.rs:1367`).
- **FUTEX_CLOCK_REALTIME on a non-wait command (F16)** is served instead of failing with `ENOSYS`.
- **Wake order (F21).** Detcore wakes the most recently queued waiter first, the reverse of Linux's FIFO order among equals. The result is deterministic, but it differs from Linux's fairness: a program that relies on FIFO handoff, such as a ticket-like protocol, can starve a waiter under Hermit.
- **Wake counts at or below zero (F21).** As described under F21: 0 for `val == 0` where Linux likely wakes one, and a likely panic for a negative `val` when waiters are present.
- **Signals (F19).** No path was found that interrupts a parked futex waiter with a signal. A guest whose only wake is a signal handler would then never wake.
- **Unmapped private wake (F22)** likely returns `EFAULT` where Linux returns 0.

Target (owner decision 10): guest input never stops the tracer.

- **Now.** Replace both `panic!` arms, `libc::FUTEX_FD` and `other`, in the precise path (`detcore/src/syscalls/threads.rs:1367` to `detcore/src/syscalls/threads.rs:1371`) and in the polling path (`detcore/src/syscalls/threads.rs:1434` to `detcore/src/syscalls/threads.rs:1438`) with `ENOSYS` to the guest (F23). For `FUTEX_FD` and undefined commands that is already Linux's answer. For the requeue, wake-op and PI commands it is the answer of a kernel without them; musl's condition variables requeue (`FUTEX_REQUEUE` in `unlock_requeue`, musl `src/thread/pthread_cond_timedwait.c`) and ignore its result, so with `ENOSYS` a musl waiter can be left asleep with no wake issued, which is why the requeue commands are modelled first. The refusal must come before the word read at `detcore/src/syscalls/threads.rs:1236`, which today turns an unmapped `uaddr` into `EFAULT` for every command.
- **Then.** Model `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` and `FUTEX_WAKE_OP` in the existing queue (F24): move waiters between `FutexID` keys, and do the `FUTEX_WAKE_OP` read-modify-write of `*uaddr2` while the turn is held. The PI family follows later; until then it keeps `ENOSYS`.
- **Other guest-reachable panic.** A negative wake count reaches `max_to_wake.try_into().unwrap()` at `detcore/src/scheduler.rs:2840` when waiters are present (F21). Under decision 10 this is a defect too; by the F21 reading Linux still wakes one waiter for such a count, so the fix is to follow Linux, not to refuse.

Target (owner decision 6): the model owes Linux's checks in the kernel's order. In `SYSCALL_DEFINE6(futex, ...)` (`kernel/futex/syscalls.c`) the timeout is copied and validated first (`EFAULT`, then `EINVAL` from `futex_init_timeout`), and only for the commands that take one (`futex_cmd_has_timeout`). Then `do_futex` refuses `FUTEX_CLOCK_REALTIME` on a command that does not accept it with `ENOSYS` (F16), and its command switch gives `ENOSYS` for anything else unknown (F20). The per-command checks (`bitset == 0`, alignment, the word compare) come last. The list under Linux semantics puts the clock check before the timeout; that differs only for `FUTEX_LOCK_PI|FUTEX_CLOCK_REALTIME` with a bad timeout pointer, where Linux gives `EFAULT`. Today Detcore masks the clock flag off (`detcore/src/syscalls/threads.rs:1267`) and reads the word before any check (`detcore/src/syscalls/threads.rs:1236`).

Target (owner decision 13): absolute deadlines stay on virtual time, and must follow the clock. Today an absolute `FUTEX_WAIT_BITSET` deadline is used directly as a logical time (`detcore/src/syscalls/threads.rs:798` to `detcore/src/syscalls/threads.rs:802`), which is right only because `handle_clock_gettime` gives every clock id the same value (`detcore/src/syscalls/time.rs:204`). When clock ids get distinct virtual clocks, the deadline must be read on `CLOCK_REALTIME` with `FUTEX_CLOCK_REALTIME` (F16) and on `CLOCK_MONOTONIC` without it.

`linux_determinism` stays `nondeterministic` because these are properties of Linux. Under Hermit the scheduling, waiter-choice and timeout sources become logical, and the signal source is open (F19).

## Recommended tests

- T1 (covers F1, F2, F18, F21): under `hermit run --verify` on ptrace with sequentialization, N threads `FUTEX_WAIT` on one word. The main thread checks that a mismatched `val` gives `EAGAIN`, then issues `FUTEX_WAKE` with `val = 2` and asserts the return is 2. Print the identities of the woken threads and require identical output across runs.
  Existing candidates:
  - `tests/rust/futex_wake_some.rs`, run as `default_cargo_futex_wake_some` (`hermit-cli/tests/hermit_modes.rs:902`). That mode passes `--no-sequentialize-threads` (`hermit-cli/tests/hermit_modes.rs:441`), so it exercises the kernel path at `detcore/src/syscalls/threads.rs:1242`, not the model.
  - The same guest under record and replay as `record_rs_futex_wake_some` (`hermit-cli/tests/record_replay.rs:2326`).
  - The Detcore test module `futex_wait_parent` (`detcore/tests/parallelism/mod.rs:311`).
- T2 (covers F11, F12, F17): `FUTEX_WAIT_BITSET|FUTEX_PRIVATE_FLAG` with an absolute deadline times out with `ETIMEDOUT` after the expected logical interval and leaves no queued waiter. `FUTEX_WAKE_BITSET` wakes only waiters with an intersecting bitset. `val3 == 0` gives `EINVAL` for both commands.
  Existing candidates:
  - `futex_wait_bitset_timeout_is_absolute_and_removes_waiter` (`detcore/tests/misc/mod.rs:1771`).
  - `shared_futex_modes_are_supported_and_validate_bitsets` (`detcore/tests/misc/mod.rs:1603`).
  - Scheduler unit test `futex_wake_bitset_only_selects_intersecting_waiters` (`detcore/src/scheduler.rs:7643`).
- T3 (covers F15): a shared futex in a MAP_SHARED anonymous mapping, and one in a MAP_SHARED file mapping, wakes across `fork`. A `FUTEX_PRIVATE_FLAG` wait on the same mapping in two processes is not woken by the other process's private wake.
  Existing candidate: `shared_anonymous_futex_wakes_across_processes` (`detcore/tests/misc/mod.rs:1630`), anonymous mapping only.
- T4 (covers F16, F17): a relative `FUTEX_WAIT` timeout gives `ETIMEDOUT`. An absolute `FUTEX_WAIT_BITSET|FUTEX_CLOCK_REALTIME` deadline computed from `clock_gettime(CLOCK_REALTIME)` gives `ETIMEDOUT`, as does one computed from `CLOCK_MONOTONIC` without the flag. `tv_nsec = 1000000000` and negative fields give `EINVAL`. Run the absolute cases under SaBRe as well as ptrace.
  Existing candidates:
  - Unit tests `futex_timeout_units_and_modes_match_linux` (`detcore/src/syscalls/threads.rs:2986`), `absolute_futex_timeout_detects_host_and_logical_clock_domains` (`detcore/src/syscalls/threads.rs:3036`) and `futex_timeout_rejects_invalid_timespecs` (`detcore/src/syscalls/threads.rs:3061`).
  - Guest `tests/rust/futex_timeout.rs`, run unsequentialized as `default_cargo_futex_timeout` (`hermit-cli/tests/hermit_modes.rs:900`) and under record and replay as `record_rs_futex_timeout` (`hermit-cli/tests/record_replay.rs:2324`).
- T5 (covers F3, F16, F20): with raw `syscall(SYS_futex, ...)`, call op 14, op `0x7f`, `FUTEX_FD` and `FUTEX_WAKE|FUTEX_CLOCK_REALTIME`, each with a valid word. Linux returns `ENOSYS` for every case. Today Hermit panics for the first three and returns 0 for the last. Assert `ENOSYS` for all four (owner decision 10, F16), and that the guest runs to its normal exit.
- T6 (covers F4, F5, F6): once modelled, `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE` move waiters between two words, and `FUTEX_CMP_REQUEUE` returns `EAGAIN` on a mismatch. Check each returned count. `FUTEX_WAKE_OP` is exercised for every `op` value, with and without `FUTEX_OP_OPARG_SHIFT`, and for every `cmp` value. Assert `*uaddr2` after the call and the woken count on each word. An undefined `op` or `cmp` gives `ENOSYS`. A musl-linked condition-variable program is the realistic workload for F5. Until the commands are modelled, T13 asserts `ENOSYS` for them.
- T7 (covers F7, F8, F9, F10, F13, F14): a `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` mutex under contention, plus a condition variable on such a mutex for the requeue-PI pair, under `hermit run --verify`. Raw calls cover `EDEADLK`, `EPERM` from a non-owner `FUTEX_UNLOCK_PI`, `FUTEX_TRYLOCK_PI` on a free and on a held word, and `uaddr == uaddr2` giving `EINVAL`. Today each command panics. Until the PI family is modelled, assert `ENOSYS` from each raw call and that the run continues (F23); then assert the modelled behaviour.
- T8 (covers F19): a thread parked in `FUTEX_WAIT` with no timeout receives a signal whose handler sets a flag. Without `SA_RESTART`, Linux returns `EINTR`; with `SA_RESTART`, the wait restarts. Record whether Hermit delivers the signal and ends the wait, or hangs, since no signal path for futex waiters was found.
  Related existing tests for killed threads, not signals: `late_futex_rpc_after_thread_removal_returns_eintr` (`detcore/src/tool_global.rs:6423`), `pending_futex_request_woken_by_logical_kill_is_terminal` (`detcore/src/tool_global.rs:6903`), and `sabre_exit_group_cancels_parked_futex_thread` (`hermit-cli/tests/hermit_modes.rs:795`, SaBRe only).
- T9 (covers F22): run each case natively and under Hermit, and compare:
  - `FUTEX_WAIT` on a null and on an unmapped `uaddr` gives `EFAULT`;
  - a misaligned `uaddr` gives `EINVAL`;
  - a private `FUTEX_WAKE` on an unmapped address returns 0 natively.

  Under Hermit, the private wake is expected to return `EFAULT`, by reading of `detcore/src/syscalls/threads.rs:1236`.
- T10 (covers F1, F2, F11, F12, F18): an E2E glibc workload with a mutex and condition variables repeats under strict verification on ptrace.
  Existing candidate: cell `determinism-stress-c/producer-consumer` (`tests/e2e/manifests/determinism-stress-c.yaml:442`, verify ci true on ptrace at `tests/e2e/manifests/determinism-stress-c.yaml:460`), program `tests/e2e/determinism-stress/producer_consumer.c`. It reaches futex only indirectly through glibc. It is the only candidate found that runs ptrace verify in CI.
- T11 (covers F1, F2, F15): record and replay a parent and child that meet on a futex. Replay must reproduce the same wake order and output.
  Existing candidate: `record_rs_futex_wait_child` (`hermit-cli/tests/record_replay.rs:2325`), guest `tests/rust/futex_wait_child.rs`.
- T12 (covers F2, F21): with two waiters parked, call `FUTEX_WAKE` with `val = 0`, then with `val = -1`, then with `INT_MAX`. Compare the returns and woken counts with native Linux. The expected divergences, both hypotheses, are 0 against 1 for `val = 0` and a panic at `detcore/src/scheduler.rs:2840` for `val = -1`. Also assert which waiter the first `val = 1` wake picks, to make the order difference against Linux visible.
- T13 (covers F23, F3, F20): under `hermit run --verify` on ptrace, call raw `syscall(SYS_futex, ...)` with each command Hermit does not model (`FUTEX_LOCK_PI`, `FUTEX_LOCK_PI2`, `FUTEX_UNLOCK_PI`, `FUTEX_TRYLOCK_PI`, `FUTEX_WAIT_REQUEUE_PI`, `FUTEX_CMP_REQUEUE_PI`, and `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` and `FUTEX_WAKE_OP` until modelled), plus `FUTEX_FD` and op 14, once with a valid word and once with an unmapped `uaddr`. Assert `ENOSYS` every time, that the guest prints its final line and exits 0, and that Hermit's exit status is the guest's, not 125.
- T14 (covers F24, F4, F5, F6): once modelled, a musl-linked program in which N threads wait on a condition variable and the main thread calls `pthread_cond_broadcast`, under `hermit run --verify`, wakes every waiter and prints identical output across runs. Raw calls assert `EINVAL` for a negative `val` and a negative `val2` in `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE`, `EAGAIN` for a `FUTEX_CMP_REQUEUE` mismatch, `ENOSYS` for an undefined `FUTEX_WAKE_OP` `op` and `cmp`, and `EFAULT` for an unmapped `uaddr2`; compare each with the native result.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore owns the wait queue for WAIT, WAKE and the BITSET pair; requeue, WAKE_OP, PI and unknown commands panic and need modelling or a deliberate refusal); linux_determinism nondeterministic, replacing the conditional placeholder; 22 flag behaviours and 12 tests listed; open questions for the owner on the interim answer for unhandled commands, Linux wake order, signal delivery to parked waiters and the default-mode futex tests; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 10, 6 and 13 applied (ENOSYS instead of the panics at threads.rs:1367-1371 and 1434-1438, then model FUTEX_REQUEUE, FUTEX_CMP_REQUEUE and FUTEX_WAKE_OP; kernel check order; absolute deadlines per clock); target_policy stays model; new F23, F24, T13, T14; T5, T6, T7 now assert ENOSYS in the interim; removed the question on the interim answer for unhandled commands

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `futex`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex&body=Discussion+for+the+%60futex%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `futex`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should Detcore match Linux's FIFO wake order among equal waiters, replacing its last-in-first-woken order (`detcore/src/scheduler.rs:2809`)? Both are deterministic; FIFO would make Hermit's schedules closer to what programs see natively.
- How should a signal reach a thread parked in a precise futex wait (F19)? The question is whether the cross-task signal drain (`detcore/src/scheduler.rs:5263`) should cover futex waiters, as it covers waitid and restartable internal I/O. T8 should establish today's behaviour first.
- The default-mode guests `default_cargo_futex_*` (`hermit-cli/tests/hermit_modes.rs:899`) run with `--no-sequentialize-threads`, so they test kernel futexes, not Detcore's model. Should they also run in strict or verify mode, so they count as coverage for F1, F2 and F17?
- `val <= 0` and the unmapped private wake (F21, F22) are readings of the code, not measurements; T12 and T9 confirm or refute them.
