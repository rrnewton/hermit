---
schema: syscall-pages/v1
syscall: rt_sigprocmask
nr_x86_64: 14
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigprocmask&body=Discussion+for+the+%60rt_sigprocmask%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigprocmask.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigprocmask%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 6
---

## Summary

`rt_sigprocmask` reads and changes the calling thread's set of blocked signals. The kernel keeps the mask, and Hermit forwards every call to it after the kernel's own size and pointer checks, except that it removes `SIGSTKFLT` from any set the guest asks to block, because Hermit uses that signal as its preemption timer.

## Linux semantics

`rt_sigprocmask(how, set, oldset, sigsetsize)` is the raw form behind `sigprocmask(2)` and `pthread_sigmask(3)`. On x86_64 the mask is one 64-bit word, so `sigsetsize` must be 8. The call returns 0 or `-1` with `errno`.

- The mask belongs to one thread. A new thread or a `fork` child starts with its creator's mask, and `execve` keeps it, so a program's starting mask depends on the process that launched it.
- A blocked signal stays pending instead of being delivered. A standard signal raised several times while blocked is pending once; realtime signals queue.
- When the call unblocks a pending signal, the kernel delivers it on the way back to user space, so the handler runs before `rt_sigprocmask` appears to return.
- The kernel copies `set` in, applies it, and only then writes the old mask to `oldset`. An `EFAULT` on `oldset` therefore comes after the mask has already changed.

### Flags and options

- F1: `SIG_BLOCK` adds the signals in `set` to the mask.
- F2: `SIG_UNBLOCK` removes the signals in `set` from the mask.
- F3: `SIG_SETMASK` replaces the mask with `set`.
- F4: `set` NULL leaves the mask unchanged and ignores `how`, so the call is a pure query when `oldset` is given.
- F5: an unknown `how` with a non-NULL `set` gives `EINVAL` and leaves the mask unchanged; the kernel copies `set` first, so an inaccessible `set` gives `EFAULT` instead.
- F6: `oldset`, when not NULL, receives the mask as it was before the call.
- F7: `SIGKILL` and `SIGSTOP` cannot be blocked; they are dropped from `set` without an error.
- F8: `sigsetsize` other than 8 gives `EINVAL` before either pointer is read.
- F9: an inaccessible `set` gives `EFAULT` with the mask unchanged; an inaccessible `oldset` gives `EFAULT` after the mask has changed.
- F10: the mask is per thread, inherited by new threads and `fork` children, and kept across `execve`.
- F11: unblocking a pending signal delivers it before the call returns; a standard signal raised several times while blocked is delivered once.
- F12: glibc's wrappers silently drop signals 32 and 33, which glibc keeps for itself; the raw call blocks them like any other signal.
- F13: on Linux, signal 16 (`SIGSTKFLT`) is an ordinary signal: it can be blocked, and `oldset` and later queries report it as blocked.

## Determinism analysis

`linux_determinism` is `conditional`. The mask changes only through the thread's own calls, signal handler entry and return, and the calls that take a temporary mask (`rt_sigsuspend`, `ppoll`, `pselect6`, `epoll_pwait`). Given the same starting mask, every value the call returns is a function of the thread's earlier calls. The sources that break this:

- Inherited mask. `execve` keeps the mask, so a query returns whatever the launching shell, service manager or CI runner left blocked.
- Pending signals at unblock time. Whether a handler runs inside a `SIG_UNBLOCK` or `SIG_SETMASK` call (F11) depends on which signals are pending, which depends on when other threads, processes and timers sent them.
- Threads in one process each have their own mask, so which thread receives a process-directed signal depends on the masks at the moment of delivery and on scheduling.

## Hermit design

Target policy: `model`. The kernel keeps the masks, and Detcore does not shadow them. Detcore keeps the kernel's argument checks while it keeps its own preemption signal deliverable. `handle_rt_sigprocmask` at `detcore/src/syscalls/signal.rs:499` (tagged `TODO-HUMAN-REVIEW(#1046)` at `detcore/src/syscalls/signal.rs:497`):

- Checks `sigsetsize` first (`detcore/src/syscalls/signal.rs:505`), so `EINVAL` comes before any `EFAULT` (F8).
- Forwards `SIG_UNBLOCK` and any unknown `how` unchanged (`detcore/src/syscalls/signal.rs:507-508`). Neither can block `SIGSTKFLT`, and the kernel's own `EINVAL` covers F5.
- For `SIG_BLOCK` and `SIG_SETMASK` with a `set`, reads the set through `read_kernel_sigset` (`detcore/src/syscalls/signal.rs:140`). That helper first injects the same `set` with `how = -1`, which the kernel copies and then rejects with `EINVAL`, so an `EFAULT` comes from the kernel's own copy and not from a ptrace read that can see a protected page (F9). Detcore then clears the `SIGSTKFLT` bit (`without_perf_event_signal`, `detcore/src/syscalls/signal.rs:126`), pushes the result on the guest stack, and injects the modified call with the guest's own `oldset` (`detcore/src/syscalls/signal.rs:509-521`). The kernel still writes `oldset`, so F6 and the `oldset` half of F9 are the kernel's behaviour.
- With a NULL `set`, forwards the query unchanged (`detcore/src/syscalls/signal.rs:523`).
- Uses `inject_with_retry` on every path, so a preemption by the tracer is retried instead of surfacing as `ERESTARTSYS` (`detcore/src/syscalls/signal.rs:519-520`).
- Does not go through `record_or_replay`: signals are not recorded (`detcore/src/syscalls/signal.rs:528-529`), and in record and replay the masks are rebuilt by re-executing the guest's own calls.
- When a signal arrives during a blocking syscall that Detcore emulates, reads the guest's mask from the kernel (`block_signals_for_disposition`, `detcore/src/syscalls/threads.rs:640`, called at `detcore/src/syscalls/threads.rs:1681-1682`) and skips signals the guest has blocked (`detcore/src/syscalls/threads.rs:590`), then restores the mask (`restore_signals_after_disposition`, `detcore/src/syscalls/threads.rs:666`).
- Charges `FAST_NS` of logical time (`detcore/src/syscall_time.rs:43-45`).

The scheduler orders the threads of the guest, which makes the second and third sources in the analysis repeatable for signals that Hermit itself generates. The inherited mask stays an outside input.

Why `model`: the kernel must hold the mask, because it delivers the signals, so `emulate` is not possible. `passthrough` would let a guest block `SIGSTKFLT` and stop preemption. `refuse` would break every program that uses threads or signal handlers.

Where Hermit differs from Linux today (F13): a guest can never block `SIGSTKFLT`, and `oldset` and queries never show it blocked. The lit test `detcore/tests/lit/rt_sigprocmask/hermit-run.lit` pins "SIGSTKFLT is not masked" under Hermit for a full set with `SIG_BLOCK` and with `SIG_SETMASK`, where the native run says "masked". The unit test `reserved_signal_is_removed_from_the_kernel_sized_mask_only` at `detcore/src/syscalls/signal.rs:861` checks the bit clearing.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F6): `SIG_SETMASK` to block `SIGUSR1` and `SIGUSR2`, a NULL-`set` query, `SIG_UNBLOCK` of `SIGUSR1`, `SIG_BLOCK` of `SIGUSR1`, then clear the mask, checking `oldset` and the query after each step.
  Existing candidate: `tests/c/sigprocmask_state.c` (cell `c-programs/sigprocmask-state`, `tests/e2e/manifests/c-programs.yaml:18101`, verify ci on ptrace).
- T2 (covers F5, F4): a raw call with `how = 3` and a valid `set` returns `EINVAL` and the mask is unchanged; with `how = 3` and a NULL `set` it returns 0; with `how = 3` and an inaccessible `set` it returns `EFAULT`.
- T3 (covers F7): `SIG_SETMASK` with a full set, then a query: `SIGKILL` and `SIGSTOP` are not in it, and `kill(getpid(), SIGKILL)` from a child still ends the process.
- T4 (covers F8, F9): raw calls check `EINVAL` for `sigsetsize` 9 with an inaccessible `set`, `EFAULT` for `set` on a `PROT_NONE` page, a valid `set` ending exactly at a page boundary, and `EFAULT` for an inaccessible `oldset` with a check that the mask did change.
  Existing candidate: `tests/c/signal_delivery_sequence.c` (cell `c-programs/signal-delivery-sequence`, `tests/e2e/manifests/c-programs.yaml:17172`, verify ci on ptrace): size-before-pointer at line 254, inaccessible `set` at line 315, page-boundary `set` at lines 382-390. It does not test an inaccessible `oldset`.
- T5 (covers F10): a blocked signal stays blocked in a new thread, in a `fork` child and after `execve`; four threads changing their own masks concurrently do not see each other's changes.
  Existing candidates: `signal_masks_survive_fork_and_clone` at `hermit-cli/tests/signal_determinism.rs:384` and `pending_signal_and_mask_survive_exec` at `hermit-cli/tests/signal_determinism.rs:408` (they choose their backend with `format!("--backend={backend}")` at `hermit-cli/tests/signal_determinism.rs:191`, so they cannot be coverage tokens); `tests/c/sigmask_preemption.c` (cell `c-programs/sigmask-preemption`, `tests/e2e/manifests/c-programs.yaml:8070`, verify ci on ptrace), which runs `pthread_sigmask` `SIG_BLOCK`/`SIG_SETMASK` in a loop on several threads.
- T6 (covers F11, F1, F3): block `SIGUSR1`, raise it five times, check that it is pending once, restore the mask with `SIG_SETMASK`, and check that the handler ran exactly once before the call returned.
  Existing candidate: `main` in `tests/c/signal_delivery_sequence.c:425`.
- T7 (covers F12): block signals 32 and 33 with a raw call and read them back as blocked; do the same through `sigprocmask(3)` and check that glibc dropped them.
- T8 (covers F13): block `SIGSTKFLT` alone and in a full set, then query. Record what Hermit reports: by the code and the lit test, `SIGSTKFLT` is never shown as blocked, where Linux shows it. This documents the divergence; whether it should change is the open question below.
  Existing candidate: `detcore/tests/lit/rt_sigprocmask/main.c` with `detcore/tests/lit/rt_sigprocmask/hermit-run.lit`, which runs with `--no-sequentialize-threads --no-deterministic-io` rather than under `--verify`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel keeps the per-thread mask; Detcore keeps the kernel's size and pointer checks and strips SIGSTKFLT from SIG_BLOCK and SIG_SETMASK sets); linux_determinism stays conditional (inherited mask, pending signals at unblock time); 13 flag behaviours and 8 tests listed; open question for the owner on the SIGSTKFLT divergence, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T6 covered by tests/c/signal_delivery_sequence.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigprocmask`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigprocmask&body=Discussion+for+the+%60rt_sigprocmask%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigprocmask.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigprocmask%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `rt_sigprocmask`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should a guest be able to see `SIGSTKFLT` as blocked? Today Hermit silently drops it from every set the guest blocks (F13), because Reverie uses it as the preemption timer. A guest that blocks all signals and then checks its mask sees a difference from Linux; `detcore/tests/lit/rt_sigprocmask/hermit-run.lit` pins it. This is the same contract question as for `rt_sigaction` (the comment at `detcore/src/syscalls/signal.rs:79-85` calls it "genuinely open"); one possible answer is a virtual bit that `oldset` reports while the kernel mask leaves the signal deliverable. The owner decides.
