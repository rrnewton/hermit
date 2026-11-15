---
schema: syscall-pages/v1
syscall: rt_sigaction
nr_x86_64: 13
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigaction&body=Discussion+for+the+%60rt_sigaction%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigaction.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigaction%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`rt_sigaction` reads and changes the disposition of one signal: default, ignore, or a handler with flags and a mask. The kernel keeps the disposition table, and Hermit forwards every call to it after checking the arguments the way the kernel does. The exception is `SIGSTKFLT`, which Hermit reserves for its own preemption timer and turns into a no-op.

## Linux semantics

`rt_sigaction(signum, act, oldact, sigsetsize)` is the raw form behind `sigaction(3)` and `signal(3)`. On x86_64 the kernel `struct sigaction` is four 8-byte words: handler, flags, restorer and a 64-bit mask, so `sigsetsize` must be 8. The call returns 0 or `-1` with `errno`.

- If `act` is not NULL, the kernel copies it in before anything else; if `oldact` is not NULL, it writes the previous disposition after that. The two pointers may be the same.
- Dispositions belong to the process: every thread created with `CLONE_SIGHAND` shares them. `fork` copies them. `execve` resets every handler to `SIG_DFL` but keeps `SIG_IGN`, so a program's starting dispositions depend on the process that launched it.
- The disposition decides what happens when a signal is delivered later, and how an interrupted blocking syscall returns. Delivery itself, and its timing, belong to the calls that raise and unblock signals.

### Flags and options

- F1: `act` NULL makes the call a query; `oldact` NULL skips the write-back; with both NULL the call only validates `signum`.
- F2: handler `SIG_DFL` restores the default action of the signal (terminate, core, ignore, stop or continue).
- F3: handler `SIG_IGN` ignores the signal and discards any pending instance of it; for `SIGCHLD` it also makes children reap themselves.
- F4: a handler address installs a function, which runs with the signal itself and `sa_mask` added to the thread's blocked set.
- F5: `SA_SIGINFO` passes a `siginfo_t` and a `ucontext_t` to a three-argument handler.
- F6: `SA_RESTART` restarts most blocking syscalls the handler interrupted, instead of failing them with `EINTR`.
- F7: `SA_NODEFER` does not block the signal itself while its handler runs, so the handler can be re-entered.
- F8: `SA_RESETHAND` resets the disposition to `SIG_DFL` when the handler is entered.
- F9: `SA_ONSTACK` runs the handler on the alternate stack set with `sigaltstack`.
- F10: `SA_NOCLDSTOP` and `SA_NOCLDWAIT` change `SIGCHLD` only: no `SIGCHLD` for stopped or continued children, and children that reap themselves.
- F11: `SA_RESTORER` with `sa_restorer` gives the return trampoline; on x86_64 a handler installed without it cannot be delivered, and the thread gets `SIGSEGV` instead.
- F12: `sa_mask` lists the signals blocked while the handler runs; `SIGKILL` and `SIGSTOP` in it are silently dropped.
- F13: `SIGKILL` and `SIGSTOP` reject a new action with `EINVAL`, but can be queried and report `SIG_DFL`.
- F14: a `signum` outside 1..64 gives `EINVAL`. glibc keeps 32 and 33 for itself and refuses them in `sigaction(3)`; the raw call accepts them.
- F15: `sigsetsize` other than 8 gives `EINVAL` before either pointer is read.
- F16: an inaccessible `act` or `oldact` gives `EFAULT`; `act` is copied completely before `oldact` is written.
- F17: unknown `sa_flags` bits are accepted; since Linux 5.11 they are cleared in the action read back, so a program can test for support with `SA_UNSUPPORTED`.
- F18: on Linux, signal 16 (`SIGSTKFLT`) is an ordinary signal: its handler is installed, reported by later queries, and invoked on delivery.
- F19: on Linux, a `SIGTRAP` handler is invoked when the process raises `SIGTRAP` or hits a breakpoint instruction.

## Determinism analysis

`linux_determinism` is `conditional`. The disposition table changes only when the program itself calls `rt_sigaction`, a handler with `SA_RESETHAND` runs, or `execve` resets it. Given the same starting table, every result is a function of the program's earlier calls. The sources that break this:

- Inherited dispositions. `SIG_IGN` survives `execve`, so a query returns whatever the launching shell, service manager or CI runner left ignored (for example `SIGPIPE` or `SIGHUP` under `nohup`).
- Threads racing to set the same signal. The last writer wins, and which one is last depends on scheduling.
- `SA_RESETHAND` combined with asynchronous delivery. Whether a later query sees the handler or `SIG_DFL` depends on whether a signal arrived in between, which depends on the sender's timing.

Signal delivery order and timing are not part of this call; they are analysed with `kill`, `tgkill`, the timers and `rt_sigprocmask`.

## Hermit design

Target policy: `model`. The kernel keeps the disposition table, and Detcore does not shadow it. Detcore's part is to keep the kernel's argument checks while it hides the one signal it uses itself. `handle_rt_sigaction` at `detcore/src/syscalls/signal.rs:433`:

- Checks `sigsetsize` first (`detcore/src/syscalls/signal.rs:441`, via `validate_kernel_sigset_size` at `detcore/src/syscalls/signal.rs:118`), so `EINVAL` comes before any `EFAULT`, as F15 requires.
- Copies the action before it looks at `signum` (`detcore/src/syscalls/signal.rs:447`). `read_kernel_sigaction` (`detcore/src/syscalls/signal.rs:169`) first injects the same action against `SIGKILL`, which the kernel copies completely and then rejects with `EINVAL`. Any `EFAULT` therefore comes from the kernel's own copy, not from a ptrace read that could see a protected page (F16).
- Logs `HERMIT_APPROPRIATED_SIGNAL` when the guest installs a handler for `SIGTRAP` or `SIGSTKFLT` (`detcore/src/syscalls/signal.rs:456`; `warn_appropriated_signal` at `detcore/src/syscalls/signal.rs:106`).
- For `SIGSTKFLT`, which Reverie uses as `PERF_EVENT_SIGNAL` (`detcore/src/syscalls/signal.rs:460`), installs nothing and returns 0 (`detcore/src/syscalls/signal.rs:477`). If `oldact` is set, it injects a query of `SIGKILL` instead (`detcore/src/syscalls/signal.rs:474`), so the guest reads four zero words, the virtual default, with the kernel's own `EFAULT` behaviour.
- For every other signal with an action, removes `SIGSTKFLT` from `sa_mask` in a private copy pushed on the guest stack and injects that (`detcore/src/syscalls/signal.rs:484-490`). The guest's input object is not modified, so a read-only `act` still works. A query is injected unchanged (`detcore/src/syscalls/signal.rs:492`).
- Uses `guest.inject` rather than `record_or_replay`: signals are not recorded (`detcore/src/syscalls/signal.rs:528-529`), and the recorder and replayer have no `rt_sigaction` arm. In record and replay the disposition table is rebuilt by re-executing the guest's own calls.
- Reads the disposition back from the kernel when a signal arrives during a blocking syscall Detcore emulates, and uses `SIG_IGN`, `SIG_DFL` and `SA_RESTART` to choose between continuing, restarting and `EINTR` (`detcore/src/syscalls/threads.rs:599-617`). That is how F6 holds for blocking calls the scheduler owns.
- Charges `FAST_NS` of logical time (`detcore/src/syscall_time.rs:43-45`).

On the launch path where the guest program is PID 1 of the container, Hermit deliberately leaves the inherited dispositions alone, because changing them "would alter the behaviour being observed" (`hermit-cli/src/bin/hermit/run.rs:4610-4616`). The first source in the analysis therefore stays an input from outside.

Why `model`: the table must live in the kernel, which delivers the signals, so `emulate` is not possible. `passthrough` would let a guest install a handler for, or block, `SIGSTKFLT` and break preemption. `refuse` would break every program that handles a signal.

Where Hermit differs from Linux today, by design and measured in the comment at `detcore/src/syscalls/signal.rs:46-65` (2026-08-25, every signal 1..64):

- F18: a guest `SIGSTKFLT` handler is never installed. If the signal is delivered, the default action kills the guest (exit 144). The guest's `sa_mask` never blocks `SIGSTKFLT` either; the lit test `detcore/tests/lit/rt_sigaction/hermit-run.lit` pins "SIGSTKFLT is not masked" under Hermit against "masked" natively (`detcore/tests/lit/rt_sigaction/main.c`).
- F19: a `SIGTRAP` handler is installed but never runs, because ptrace consumes every `SIGTRAP`; the guest then exits 0 with no diagnostic other than the log line above.
- Realtime signals 34..64 fail by a different mechanism at the Reverie ptrace boundary; that is outside this call.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6): install `SIG_IGN`, a plain handler, and a `SA_SIGINFO|SA_RESTART` handler on `SIGUSR1`, read each back, then restore `SIG_DFL`.
  Existing candidates: `tests/c/sigaction_state.c` (cell `c-programs/sigaction-state`, `tests/e2e/manifests/c-programs.yaml:18033`, verify ci on ptrace) and `tests/c/signal_disposition.c` (cell `c-programs/signal-disposition`, `tests/e2e/manifests/c-programs.yaml:17965`, verify ci on ptrace), which also round-trips `SA_RESTART` and an `sa_mask` containing `SIGHUP`.
- T2 (covers F4, F12): deliver a signal whose handler has `sa_mask` set, and inside the handler read the blocked set with `rt_sigprocmask`: the signal itself and the `sa_mask` signals must be blocked, and `SIGKILL` in `sa_mask` must be dropped.
  Existing candidate: `detcore/tests/lit/rt_sigaction/main.c`, which checks `SIGHUP` and `SIGALRM` inside a `SIGALRM` handler but runs with `--no-sequentialize-threads --no-deterministic-io` rather than under `--verify`.
- T3 (covers F6): a handler with `SA_RESTART` interrupts a blocking `read` on a pipe, and the `read` completes; without `SA_RESTART` it fails with `EINTR`.
  Existing candidates: `signal_restarts_emulated_blocking_read` at `hermit-cli/tests/signal_determinism.rs:252` and `signal_interrupts_emulated_blocking_read` at `hermit-cli/tests/signal_determinism.rs:244`. They select their backend with `format!("--backend={backend}")` (`hermit-cli/tests/signal_determinism.rs:191`), so they cannot be recorded as coverage tokens.
- T4 (covers F7, F9): a `SA_NODEFER` handler re-enters itself, and a `SA_ONSTACK` handler runs on the alternate stack.
  Existing candidates: `signal_handler_reentrance_is_deterministic` at `hermit-cli/tests/signal_determinism.rs:392` and `alternate_signal_stack_is_preserved` at `hermit-cli/tests/signal_determinism.rs:400`, which run `tests/c/signal_determinism.c` (`SA_NODEFER` at line 713, `SA_ONSTACK` at line 769). The CI cell `c-programs/signal-determinism` (`tests/e2e/manifests/c-programs.yaml:8132`) runs only the `itimer-delivery` scenario on ptrace.
- T5 (covers F8): a `SA_RESETHAND` handler sees `SIG_DFL` when it queries its own signal, and a second delivery takes the default action.
- T6 (covers F10, F3): with `SA_NOCLDSTOP`, stopping and continuing a child sends no `SIGCHLD`; with `SA_NOCLDWAIT`, or `SIG_IGN` for `SIGCHLD`, `wait` fails with `ECHILD` after the child exits.
- T7 (covers F11): a raw `rt_sigaction` without `SA_RESTORER` followed by `raise` ends the process with `SIGSEGV`, identically in both runs of `hermit run --verify`.
- T8 (covers F13, F14, F15, F16): raw calls check `EINVAL` for a new `SIGKILL` action, for signal 0 and 65, and for `sigsetsize` 9 with an inaccessible `act`; `EFAULT` for `act` and `oldact` on a `PROT_NONE` page and for every partial length 1..31 of `act`; an aliased `act`/`oldact`; and a read-only `act` left unchanged.
  Existing candidate: `tests/c/signal_delivery_sequence.c` (cell `c-programs/signal-delivery-sequence`, `tests/e2e/manifests/c-programs.yaml:17172`, verify ci on ptrace): partial-access checks from line 105, size-before-pointer and `PROT_NONE` checks from line 232, the read-only action at lines 360-378. It does not try signals 0 or 65.
- T9 (covers F17): set `SA_UNSUPPORTED` together with a known flag and check that the read-back action has `SA_UNSUPPORTED` cleared and the known flag kept.
- T10 (covers F18): install a `SIGSTKFLT` handler, query it, and raise `SIGSTKFLT`. Record what Hermit does: by the code it returns 0, reports `SIG_DFL` and the guest dies with exit 144, where Linux runs the handler. This documents the divergence; whether it should change is the open question below.
  Existing partial coverage: the `SIGSTKFLT` paths in `tests/c/signal_delivery_sequence.c` (lines 149, 172, 291 and 298) check the raw ABI, not delivery.
- T11 (covers F19): install a `SIGTRAP` handler and `raise(SIGTRAP)`. Record whether the handler runs; by the measurement in `detcore/src/syscalls/signal.rs:56-60` it does not under Hermit, and the guest exits 0.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel keeps the disposition table; Detcore keeps the kernel's size and pointer checks and reserves SIGSTKFLT by a no-op and by stripping it from sa_mask); linux_determinism stays conditional (inherited SIG_IGN, racing writers, SA_RESETHAND timing); 19 flag behaviours and 11 tests listed; open question for the owner on the SIGSTKFLT and SIGTRAP divergences, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigaction`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigaction&body=Discussion+for+the+%60rt_sigaction%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigaction.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigaction%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `rt_sigaction`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- What is a guest owed for the two signals Hermit takes for itself? Today a `SIGSTKFLT` handler is silently not installed and a delivered `SIGSTKFLT` kills the guest, and a `SIGTRAP` handler is installed but never runs (F18, F19). The source comment at `detcore/src/syscalls/signal.rs:79-85` says refusing with `EINVAL` was rejected because the Go runtime installs a `SIGSTKFLT` handler, and that "the contract question of what a guest is owed here is genuinely open". This body keeps `model` for everything else; the owner decides whether these two stay documented divergences or need another mechanism (for example, a different preemption signal).
