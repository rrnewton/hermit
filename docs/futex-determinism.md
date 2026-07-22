# Futex determinism audit

Audit of Detcore's `futex(2)` handling for deterministic behavior, with the
observed gaps. `futex` is the synchronization primitive underneath every
threaded runtime (glibc/pthreads, Go, the JVM, CPython), so any nondeterminism
here undermines threading determinism generally.

Scope: read of `detcore/src/syscalls/threads.rs` and `detcore/src/scheduler.rs`
on the audited checkout, plus end-to-end runs under `hermit run` (ptrace
backend). Evidence commands and outputs are inline.

## How futexes are handled

`Detcore::handle_futex` (`detcore/src/syscalls/threads.rs`) dispatches by
`--debug-futex-mode`:

| mode | implementation | notes |
| --- | --- | --- |
| `precise` (**default**) | `handle_futex_blocking` — futex is fully **emulated** inside Detcore; the real syscall never runs in the kernel | deterministic |
| `polling` | `handle_futex_polling` — nonblocking futex retried as an internal-polling operation | deterministic |
| `external` | `record_or_replay_blocking` — real blocking syscall, recorded/replayed | timing not modeled |

When `--no-sequentialize-threads` is set, `handle_futex` injects the raw syscall
and none of this applies (no determinism guarantee).

### Operations handled (precise mode)

`FUTEX_WAIT`, `FUTEX_WAKE`, `FUTEX_WAIT_BITSET`, `FUTEX_WAKE_BITSET`. `FUTEX_FD`
is explicitly refused (removed from Linux in 2.6.26). Every other operation
falls into `other => panic!("[detcore] futex op not handled yet: {}")`
(`threads.rs:371`). See **Gaps**.

## Wake ordering — deterministic

Waiters are stored per-futex in a `Vec<FutexWaiter>` in arrival order
(`Scheduler::sleep_futex_waiter`, `scheduler.rs`), which is deterministic because
Detcore serializes the guest. A wake:

1. `wake_futex_waiters` takes the waiter list;
2. `take_matching_futex_waiters` keeps only waiters whose `bitset` intersects the
   wake mask (order-preserving `partition`), so `FUTEX_WAKE_BITSET` is honored;
3. `choose_futex_wakees` selects how many to wake:
   - default: `vec.split_off(vec.len() - n)` — a fixed slice, deterministic;
   - `--fuzz-futexes`: `partial_shuffle(&mut fuzz_prng, n)` — shuffled with the
     seeded fuzz PRNG, so still deterministic for a fixed `--fuzz-seed`.

`--chaos` perturbs the overall schedule but likewise from a fixed seed.

**Verification.** The stress test in
`hermit-cli/tests/futex_wake_order_determinism.rs` blocks `WAITERS = 6` threads
in `FUTEX_WAIT` and wakes them one at a time; each woken thread writes its id, so
stdout is the exact wake order. Under `hermit run --strict --verify` the guest is
executed twice and the run fails unless the two outputs are bitwise-identical
(assurance **L2**). Observed on the ptrace backend (`--log=error`):

| configuration | result | wake order |
| --- | --- | --- |
| `--strict --verify` | L2 pass | `5 4 3 2 1 0` |
| `--strict --verify --chaos` | L2 pass | `0 1 2 5 4 3` |
| `--strict --verify --fuzz-futexes` | L2 pass | `2 4 1 3 0 5` |

Three different orders, each bitwise-reproducible under `--verify` — wake
ordering is deterministic in all three configurations.

## Timeout — deterministic (virtual time)

`FUTEX_WAIT`/`FUTEX_WAIT_BITSET` timeouts are converted to a deadline in
`LogicalTime` (virtual time) by `futex_timeout_deadline`, and the waiter is
registered in `blocked.timed_waiters` keyed by that logical deadline
(`sleep_futex_waiter`). Timeouts therefore fire at a deterministic virtual time
and yield a deterministic `ETIMEDOUT`, independent of host wall-clock timing.
Covered by the existing `rustbin_futex_timeout` guest.

## Gaps found

1. **`FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE`, `FUTEX_WAKE_OP`, and the PI operations
   are not handled — they panic (hard crash) in the default precise mode.**
   `handle_futex_blocking`'s match has no arm for them, so they hit
   `other => panic!` at `threads.rs:372`. Evidence:

   ```text
   $ hermit run --strict -- ./requeue        # requeue does FUTEX_CMP_REQUEUE (op 4)
   thread 'main' (1) panicked at detcore/src/syscalls/threads.rs:372:17:
   [detcore] futex op not handled yet: 4
   ```

   Impact: modern glibc `pthread_cond_*` no longer uses requeue, but code paths
   that do (older glibc, direct requeue users, `FUTEX_WAKE_OP` from some
   allocators/condvars, PI mutexes with `PTHREAD_PRIO_INHERIT`) will crash the
   container rather than run nondeterministically or degrade gracefully. A
   graceful fallback (e.g. route to `external` mode or emulate requeue by moving
   waiters between futex queues) would be the fix. Not addressed here (larger
   change; this task is audit + test + document).

2. **Default wake order is not Linux-FIFO.** `choose_futex_wakees` takes the tail
   of the arrival-ordered list (`split_off(len - n)`), so waiters wake in
   roughly reverse-arrival (LIFO) order — see the `5 4 3 2 1 0` result above.
   This is fully deterministic but does not match Linux, which wakes the
   longest-waiting thread first. Programs that depend on FIFO wake fairness could
   behave differently under Hermit than on the host (a fidelity gap, not a
   determinism gap).

3. **`--chaos` starves `sched_yield` spin-wait coordination (GH #81).** A guest
   that coordinates with a `sched_yield` busy-loop (rather than a blocking wait)
   fails to make progress under `--chaos` and times out. This is not
   futex-specific but affects futex stress programs that spin to wait for peers
   to park. The stress test added here coordinates with `nanosleep`
   (virtual-time, non-spinning) to avoid it; a `sched_yield`-based variant of the
   same program times out under `--chaos`.

## Reproduction

```sh
cargo test -p hermit --test futex_wake_order_determinism   # L2 in 3 configs
# Manual wake-order inspection:
hermit --log=error run --strict                -- ./futex_wake_order   # 5 4 3 2 1 0
hermit --log=error run --strict --chaos        -- ./futex_wake_order   # 0 1 2 5 4 3
hermit --log=error run --strict --fuzz-futexes -- ./futex_wake_order   # 2 4 1 3 0 5
```
