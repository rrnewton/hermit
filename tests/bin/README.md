# Manual C regression programs

These programs are focused reproducers. Build their executables outside the
source tree or remove them before committing. The robust-futex reproducer is
also compiled by `hermit-cli/tests/threading_syscalls.rs`.

## Robust futex owner death

`robust_futex_test.c` checks the Linux robust-list contract for waiters that are
already blocked when a mutex owner, thread group, or externally killed process exits:

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
PASS: blocked and failed signals preserved live owner
PASS: pending owner-zero robust wake preserved word
PASS: robust mutex waiter received EOWNERDEAD
PASS: sibling robust-list lookup and ESRCH semantics
PASS: legacy and futex2 variants handled deterministically
PASS: exit_group and fatal-signal owner death recovered
PASS: negative process-group SIGKILL handled deterministically
```

### Hermit strict verification

The default precise futex model now mirrors each thread's robust-list head and
performs deterministic owner-death cleanup before the host thread exits. The
regression reaches L2 (ptrace backend, log level off, relaxations: none):

```text
$ timeout --kill-after 5s 30s target/release/hermit --log=off run \
    --strict --verify --base-env=minimal --max-timeslice=disabled \
    --tmp=/tmp -- ./robust_futex_test
:: Success: deterministic. Determinism verified.
```

The same fixture blocks real waiters across legacy `FUTEX_CMP_REQUEUE` and the
U32 `futex_wait`, `futex_wake`, and `futex_requeue` interfaces. It also probes
`FUTEX_WAKE_OP`, sibling `get_robust_list`, missing-thread `ESRCH`,
process-shared `exit_group`, and external `SIGKILL` cleanup.
The external `SIGKILL` case keeps an unrelated process continuously runnable
with `sched_yield` until the robust waiter observes `EOWNERDEAD`, preventing
reconciliation from depending on global run-queue idleness.
It additionally checks that a separate process can deliver `SIGKILL` through a
negative process-group selector without introducing wait-order divergence.
The Hermit integration test also invokes the fixture's
`--hermit-broadcast-only` mode inside the isolated PID namespace. That mode
waits until a child is ready, calls `kill(-1, SIGKILL)` with SIGCHLD unmasked,
and verifies the child status at L2. It first installs a SIGCHLD handler and
waits for a separate child event, guarding against suppression of caught
signals, then restores the default disposition. The broadcast targets request
`CLONE_UNTRACED` through both legacy `clone` and `clone3`; Detcore must remove
that flag so every live guest descendant remains represented in the
deterministic scheduler. The `clone3` request uses the 64-byte v0 structure at
the end of a readable page, with the following page protected, to catch fixed
88-byte argument reads. Its child also verifies that the sanitizer's scratch
mapping is not inherited through the fork-like clone. Do not run that mode
natively.

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

The current ptrace backend does not synthesize the configured signal when the
emulated timer expires. With default logging and no relaxations, this command:

```sh
target/release/hermit run --strict -- ./posix_timer_test
```

exits 1 after advancing past the bounded virtual-time deadline:

```text
FAIL: SIGALRM was not delivered within 100 ms of virtual time
```

The expected failure should become a success assertion when deterministic
`SIGEV_SIGNAL` delivery is implemented.
