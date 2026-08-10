# Manual C regression programs

These programs are focused reproducers that are not part of the generated
Cargo test manifest. Build their executables outside the source tree or remove
them before committing.

## Robust futex owner death

`robust_futex_test.c` checks the Linux robust-list contract for a waiter that is
already blocked when a mutex owner exits:

1. Thread A explicitly re-registers glibc's robust-list head with
   `set_robust_list`, then locks a `PTHREAD_MUTEX_ROBUST` mutex.
2. Thread B enters `pthread_mutex_lock` and sets the mutex's `FUTEX_WAITERS`
   bit.
3. Thread A exits without unlocking.
4. Thread B must wake and receive `EOWNERDEAD`, mark the mutex consistent, and
   unlock it.

The `FUTEX_WAITERS` check is important. Without it, Thread B could start after
Thread A exits and observe `EOWNERDEAD` from the mutex word without exercising
the owner-death wakeup.

Build the reproducer from the repository root:

```bash
cc -O2 -Wall -Wextra -Werror -pthread \
  tests/bin/robust_futex_test.c -o robust_futex_test
```

### Native control

On x86_64 Linux with glibc, the control exits 0:

```text
$ timeout 10s ./robust_futex_test
PASS: robust mutex waiter received EOWNERDEAD
```

### Historical failure (fixed)

Before Detcore modeled the owner-death protocol, this guest could not reach L1.
At `2b38d8e629ee582db6a59f340b1a3c8980fd85c5` the strict run aborted (ptrace
backend, default log level, relaxations: none):

```text
$ timeout 10s target/release/hermit run --strict -- ./robust_futex_test
Deadlock detected: thread(s) waiting on futex, but no runnable threads left.
  turn 11, committed time 1_767_225_600.010_326_205s
  run queue: 0 runnable
  threads (2), by dettid:
    dtid 3: FutexWait: R
    dtid 7: FutexWait: R
  futex waiters (2), by futex:
    private MmId { creator: DetPid(3), generation: 1 } address 0x404100: dtid 7 (bitset 0xffffffff)
    private MmId { creator: DetPid(3), generation: 1 } address 0x7ffff73fe910: dtid 3 (bitset 0xffffffff)
  ...
Error: Sandbox container exited unexpectedly
     > Process exited with code: Exited(1)
```

The run now ends promptly with a nonzero status (measured 0.02s). It previously
reported the same deadlock as a scheduler *panic* and then hung, so a host
`timeout` exited 124 with the tracees still stopped: the scheduler is a spawned
task, so its panic was captured by the task harness while every guest thread
stayed parked on a scheduler response that could no longer arrive. The verdict
is now returned to `sched_loop_inner`, which prints it and exits the container
alongside the existing `--stop-after-*` exits.

A DEBUG capture makes the missing bridge explicit:

```bash
timeout 20s target/release/hermit --log debug run --strict -- \
  ./robust_futex_test 2>/tmp/robust-futex-owner-death-debug.log
```

The waiter (`dtid 7`) blocks on the robust mutex word:

```text
inbound syscall: futex(0x404100, 0, -2147483643, NULL, NULL, 4210976) = ?
```

`-2147483643` is `0x80000005`: `FUTEX_WAITERS` plus the owner's TID 5. The owner
(`dtid 5`) then exited, and Detcore logged only its modeled
`CLONE_CHILD_CLEARTID` wake, at a different futex address. The scheduler's
deadlock dump still listed `dtid 7` in `futex_waiters` at address `4210944`
(`0x404100`). Linux's own robust-list cleanup marked the mutex and issued an
internal kernel wake, but that wake targeted the kernel futex queue, while the
precise futex model parks waiters in Detcore's own pool.

### Polling-mode diagnostic

Polling mode observes the kernel's owner-death word update instead of relying
on Detcore's precise waiter queue. It passes Stripped verification, not L2
(ptrace backend, ERROR log level, `--debug-futex-mode polling`, no determinism
relaxations):

```text
$ timeout 20s target/release/hermit --log error run --strict --verify \
    --debug-futex-mode polling -- ./robust_futex_test
:: Success: deterministic. Determinism verified.
```

### Current strict result

Detcore now replays Linux's `exit_robust_list()` when a thread exits
(`detcore/src/syscalls/robust_list.rs`), so the default precise futex model
wakes the waiter itself:

```text
$ target/release/hermit run --strict -- ./robust_futex_test
PASS: robust mutex waiter received EOWNERDEAD
```

with, at DEBUG:

```text
[detcore, dtid 5] robust-list owner death: futex word 0x404100 0x80000005 -> 0xc0000000
[detcore, dtid 5] robust-list owner death woke 1 waiter(s) on futex Private { ..., address: 4210944 }
```

The futex word after owner death, probed directly, shows which half was missing
where — the host kernel already performed the transition under ptrace, so only
the wake was absent there, while KVM performed no transition at all:

| Run | Futex word after owner death |
| --- | --- |
| native | `0x40000000` |
| ptrace, before the fix | `0x40000000` (host kernel wrote it) |
| KVM, before the fix | `0x00000004` (no write at all) |
| ptrace, after | `0x40000000` |
| KVM, after | `0x40000000` (Detcore performs it) |

Measured per backend with
`run --strict --verify --verify-strict --verify-json <path>`, all guests exiting
0 and printing the PASS line:

| Backend | Result |
| --- | --- |
| ptrace | L2 canonical, `bitwise_parity: true` (189 \| 189 INFO messages) |
| SaBRe | L2 canonical, `bitwise_parity: true` (187 \| 187 INFO messages) |
| KVM | verified; `bitwise_parity: false` because KVM compares only exit status/stdout/stderr |
| DBT | `Determinism verified` with matching guest-memory hashes; `--verify-json` still records `no_result` for the DBT comparator |
| LiteInst | unrelated backend gap: `clone3` is refused with `-524 ENOTSUPP` before the guest creates a thread |

Record/replay also matches:
`record start --strict --verify` reports `replay matched recording`
(154 \| 154 DETLOG messages).

`hermit-cli/tests/robust_futex_owner_death.rs` is the automated regression test
and asserts the ptrace L2 row above.

### Scope: voluntary thread exit only

Detcore replays the walk from the `exit` and `exit_group` syscall handlers, so it
covers a thread that exits of its own accord — which is what this guest does.
Linux walks the robust list of *every* task in a dying thread group. A sibling
torn down by `exit_group`, by `execve`'s `de_thread`, or by a fatal signal never
reaches those handlers, so its list is not replayed. Single-process guests are
unaffected, because every waiter dies with the group; a process-shared robust
mutex is not. Tracked in
<https://github.com/rrnewton/hermit/issues/2082>.

# POSIX timer signal-delivery probe

`posix_timer_test.c` arms a one-shot `CLOCK_MONOTONIC` POSIX timer for
10 ms with `SIGEV_SIGNAL` and waits up to 100 ms for `SIGALRM`.

Build the guest from the repository root:

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror \
  tests/bin/posix_timer_test.c -o posix_timer_test -lrt
```

Native Linux delivers the signal and exits 0:

```text
PASS: SIGALRM delivered after POSIX timer expiration
```

The ptrace backend synthesizes the configured signal when the virtual timer
expires. With default logging and no relaxations, this command:

```sh
target/release/hermit run --strict -- ./posix_timer_test
```

exits 0 and prints:

```text
PASS: SIGALRM delivered after POSIX timer expiration
```

The bounded failure path remains in the guest to catch lost timer events.
