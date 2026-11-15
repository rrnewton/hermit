---
schema: syscall-pages/v1
syscall: restart_syscall
nr_x86_64: 219
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/404
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+restart_syscall&body=Discussion+for+the+%60restart_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frestart_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60restart_syscall%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: unsupported
  dispatch: "unsupported"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`restart_syscall()` takes no arguments. The kernel issues it to resume a `nanosleep`/`clock_nanosleep`, timed `futex` wait or `poll` that a signal or stop interrupted. Called directly with nothing pending, it returns `EINTR`. At hermit f4055eb9cae4 it is the only syscall Detcore classifies as Unsupported (`detcore/src/syscall_classification.rs:838`). By default a run stops on its first use; with `--allow-unsupported-syscalls` it is forwarded to the kernel with a warning.

Manual reference: Linux man-pages restart_syscall(2).

## Linux semantics

Each thread has a kernel `restart_block` that user space cannot see. Its `fn` member names the function `restart_syscall` will run. Three syscall families set it, through `set_restart_fn()`, which returns the kernel-internal error `ERESTART_RESTARTBLOCK` (516). The union in `include/linux/restart_block.h` has one variant for each family: `futex`, `nanosleep` and `poll`. When nothing is pending, `fn` is `do_no_restart_syscall`, which returns `-EINTR`.

What happens to `ERESTART_RESTARTBLOCK` on the way back to user mode depends on how the signal is disposed of:

- **A user handler runs.** The call returns `EINTR`, whether or not the handler has `SA_RESTART`. `rt_sigreturn` then resets `fn` to `do_no_restart_syscall`, so nothing stays pending after the handler returns.
- **No handler runs.** This covers a signal whose action is ignore, a stop followed by `SIGCONT`, a signal the ptrace tracer suppresses, a ptrace group stop or `PTRACE_INTERRUPT`, and the freezer. The kernel puts the restart number (219 on x86-64) in `rax` and moves `rip` back two bytes, so the thread runs its own `syscall` instruction again as `restart_syscall`. A ptrace tracer sees this as a syscall-enter stop for `restart_syscall`, and seccomp filters see it as an ordinary syscall.

Two facts about ptrace matter here. First, a traced task does not drop a signal whose action is ignore, including the default-ignore `SIGCHLD`, when the signal is generated. The signal therefore interrupts a blocking call that it would not have interrupted natively. Second, the kernel records whether the interrupted call used the 64-bit or the compat ABI, so the restart runs under the original ABI.

The resumed function can itself be interrupted again. It then returns `ERESTART_RESTARTBLOCK` again, and `restart_syscall` can repeat any number of times.

### Flags and options

- F1: nothing pending (`do_no_restart_syscall`), including any direct call by a program outside a handler: returns `-1`/`EINTR` and has no other effect.
- F2: resumes an interrupted relative `nanosleep` or `clock_nanosleep` (absolute `TIMER_ABSTIME` sleeps restart through `ERESTARTNOHAND` instead). The sleep continues to the original deadline and returns 0. Another interruption with a handler gives `EINTR` and writes the remaining time to `rem`.
- F3: resumes an interrupted `futex` wait that had a timeout (`FUTEX_WAIT`, `FUTEX_WAIT_BITSET`). The kernel reads the futex word again: it returns `EAGAIN` if the value changed, 0 on a wake, and `ETIMEDOUT` at the original deadline. A wait without a timeout restarts the `futex` call itself through `ERESTARTSYS`, not through `restart_syscall`.
- F4: resumes an interrupted `poll` (`do_restart_poll`). The descriptors are scanned again with the time left before the original end time, or with no limit if the call had no timeout. `ppoll`, `select` and `pselect6` restart differently and never use `restart_syscall`.
- F5: the call reaches the kernel because one of F2-F4 was interrupted and no handler ran (see above). With a handler the result is always `EINTR`. Under ptrace this includes signals with an ignore action and signals the tracer suppresses, which do not interrupt the call natively.
- F6: a direct call made inside a signal handler, before `rt_sigreturn`, while the interrupted call's restart is still pending. It resumes that call, which has already returned `EINTR` to the program. After `rt_sigreturn`, a direct call falls back to F1.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Programs and libc do not call `restart_syscall`; it is the kernel's way to continue a blocking call.

- A direct call with nothing pending (F1) always returns `EINTR`. The fixture `tests/c/dbt_unsupported_syscall.c:22` relies on this.
- A kernel-issued restart (F2-F5) behaves exactly like the call it resumes. Whether it happens at all depends on when a signal or stop arrived during the blocking call. Its result depends on the futex word, the descriptors' readiness and the clock.
- F6 depends on the thread's hidden restart state, which in turn depends on its signal history.

### Nondeterminism sources

- When a signal or stop arrives relative to the blocking call. This decides whether the call is interrupted and whether `restart_syscall` happens at all.
- The interrupted call's own inputs: the futex value and wakers (F3), descriptor readiness (F4), and time (F2-F4).
- Signals and stops from the ptrace tracer, which interrupt calls that would not be interrupted natively (F5).
- The per-thread kernel restart state, which the program cannot read. It makes F1 and F6 differ for the same direct call.

## Hermit design

Target policy: `model`. Detcore should own the logical "pending restart" state of each thread, the way it already owns the restart behaviour of the calls it parks. Then a direct call and a kernel-issued restart both get Linux's answer without depending on hidden kernel state. The code at hermit f4055eb9cae4 does not do this yet.

What the code does today:

- `classify_syscall` lists `restart_syscall` as the single Unsupported entry (`detcore/src/syscall_classification.rs:838`). The census test pins it as the only one (`UNSUPPORTED_SYSCALLS` at `detcore/src/syscall_classification.rs:1761`; `passthru_opt_intercepts_every_unsupported_syscall` at `detcore/src/lib.rs:3123`). It is subscribed in both run mode and record/replay mode, so it always reaches Detcore.
- The Unsupported dispatch arm (`detcore/src/lib.rs:2844`) calls `handle_unsupported_syscall` (`detcore/src/lib.rs:379`). Fail-closed is the default, because `config.panic_on_unsupported_syscalls = !self.allow_unsupported_syscalls` (`hermit-cli/src/bin/hermit/run.rs:3672`) and `shutdown_on_unsupported_syscall` follows it (`hermit-cli/src/bin/hermit/run.rs:3679`). In that mode Detcore logs `unsupported syscall: restart_syscall() = ?` (`detcore/src/lib.rs:387`) and shuts the run down with the policy-refusal exit code (`detcore/src/lib.rs:395`).
- With `--allow-unsupported-syscalls` (`hermit-cli/src/bin/hermit/run.rs:452`), Detcore records the call for the aggregate warning (`report_unsupported_syscall`, `detcore/src/tool_global.rs:3224`) and forwards it through `record_or_replay` (`detcore/src/lib.rs:404`-`405`). The call still advances virtual time by the fast cost (`unsupported_syscalls_still_advance_time` at `detcore/src/syscall_time.rs:250`).
- In the default sequentialized mode, Detcore already implements the effect of `restart_syscall` for parked sleeps, inside its scheduler. `nanosleep` waits under `ParkedWaitPolicy::NanosleepNoHandlerRestart` (`detcore/src/syscalls/time.rs:303`) and `pause` under `PauseNoHandlerRestart` (`detcore/src/syscalls/signal.rs:371`). In this mode a `futex` wait never runs in the kernel (the default `--debug-futex-mode=precise`, `detcore-model/src/config.rs:597`, dispatched at `detcore/src/syscalls/threads.rs:1246`), and a `poll` with a nonzero timeout is retried as nonblocking probes (`detcore/src/syscalls/io.rs:1117`). In these paths the kernel never arms a restart block, so a guest normally meets `restart_syscall` only through a direct call.
- Several paths do leave a blocking F2-F4 call to the kernel, where an interruption with no handler would make the kernel issue `restart_syscall`. That restart would hit the fail-closed refusal although the guest never called the syscall:
  - `--no-sequentialize-threads` injects `nanosleep` (`detcore/src/syscalls/time.rs:384`) and `futex` (`detcore/src/syscalls/threads.rs:1243`).
  - The internal debug option `--debug-futex-mode=external` runs futex waits through `record_or_replay_blocking` (`detcore/src/syscalls/threads.rs:1248`).
  - In record mode, or without sequentialization, a `poll` with a nonzero timeout goes to `handle_external_poll` (`detcore/src/syscalls/io.rs:423`). That function runs the real `poll` through `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:1137`).

  This conclusion comes from reading the code plus the kernel semantics above; nobody has measured it (T3, T4). Whether the Reverie ptrace backend shows Detcore the original call's `ERESTART_RESTARTBLOCK`, or resumes the guest so that its own `syscall` instruction re-executes as `restart_syscall`, is outside this export.

The model this body proposes:

1. **No restart pending (F1).** If Detcore has not seen a pending restart for the thread, it returns `EINTR` itself. It does not consult the kernel's `restart_block`, so the answer does not depend on state the kernel keeps for the host thread.
2. **Pending restart of a call Detcore forwarded (F2-F5).** When a call that Detcore forwarded to the kernel comes back with `ERESTART_RESTARTBLOCK`, Detcore records a pending restart for that call. The next `restart_syscall` resumes it through the same blocking-external path as the original call, and record and replay treat the original call and its restart as one logical operation. Replay never runs the original `poll` or `futex` in the kernel. A recording that contains a kernel-issued `restart_syscall` event therefore cannot be replayed one event at a time, so the record format must fold the restart into the original call.
3. **Inside a handler (F6).** For a sleep that Detcore parked and a handler interrupted, the kernel never armed a restart. A forwarded direct call would therefore return `EINTR` where Linux resumes the sleep. The model resumes the parked sleep from Detcore's own deadline (`absolute_deadline` in `NanosleepNoHandlerRestart`). `rt_sigreturn` clears the pending state, as on Linux.

Why not the alternatives:

- `refuse`: a deterministic `ENOSYS` is harmless for a direct call (F1), but it would break every kernel-issued restart (F2-F5). The cause is a signal or stop the guest did not control, and the call should continue.
- `passthrough`: correct for F1 while no restart block is armed, and it resumes kernel-run calls in record mode and in non-sequentialized runs. But it cannot replay a restart whose original call replay never sent to the kernel, and it gets F6 wrong for parked sleeps. Its F1 answer also depends on host kernel state that Detcore does not track.
- `emulate` with a fixed `EINTR`: correct for F1 and wrong for every real restart. A program whose blocking call is restarted with no handler never sees `EINTR` natively.

`linux_determinism` stays `conditional`. Under the model, F1 and F6 become deterministic. F2-F5 are as deterministic as the forwarded call they resume, which Hermit already treats as external input in the modes that forward it.

## Recommended tests

- T1 (covers F1): call `syscall(SYS_restart_syscall)` directly with nothing pending, under `hermit run --verify` on ptrace. Check that it returns `-1` with `errno == EINTR`, identically in both runs, and that the run is not refused.
  Existing related coverage: the fixture `tests/c/dbt_unsupported_syscall.c:28` makes the call but discards its result. Its manifest cell `c-programs/dbt-unsupported-syscall` (`tests/e2e/manifests/c-programs.yaml:1177`) is disabled on ptrace (`ci: ptrace: false`, `tests/e2e/manifests/c-programs.yaml:1196`). The test `run_ptrace_fails_closed_by_default_on_unsupported_syscall` (`hermit-cli/tests/cli.rs:2124`) asserts today's refusal, and the `--allow-unsupported-syscalls --verify` forwarding of the same fixture; it does not check the result. All of these pin the current refusal policy and would change when the model lands.
- T2 (covers F2, F5): a child does a relative `nanosleep` and the parent sends `SIGSTOP` and then `SIGCONT`, with no handler installed. The child's sleep must return 0, with no `EINTR`, after at least the requested virtual time. Run it in the default sequentialized mode, where the sleep is parked, and with `--no-sequentialize-threads`, where it is injected and the kernel issues `restart_syscall`. Neither run may be refused. A variant with a handler must give `EINTR` and write `rem`.
- T3 (covers F3, F5): a `FUTEX_WAIT` with a timeout is interrupted by a signal whose action is ignore. Under ptrace that signal still interrupts the wait. Run it with `--no-sequentialize-threads` (an injected futex). The wait must resume and return `ETIMEDOUT` at its deadline, or 0 if another thread wakes it. Check `EAGAIN` when the futex word changed during the interruption.
- T4 (covers F4, F5): a parent sits in `poll` on a pipe with a nonzero timeout while a child exits, leaving `SIGCHLD` at its default ignore action. Run it under `hermit record` and then `hermit replay`. Recording must not be refused, `poll` must return the same value as natively, and replay must reproduce the run. This measures the record-mode exposure described under Hermit design.
- T5 (covers F6): a relative `nanosleep` is interrupted by a signal with a handler. Inside the handler, call `restart_syscall` directly: on Linux it resumes the sleep for the time remaining and returns 0. After the handler returns, a direct call must give `EINTR`. Compare Hermit on ptrace with native Linux.
- T6 (covers F1, F5): record and replay a guest that makes a direct call (F1) and a guest whose blocking call is restarted by the kernel (T4's workload). Replay must not ask the kernel for either.
  Existing related coverage: `recording_rejects_an_unsupported_syscall_by_name` (`hermit-cli/tests/record_replay.rs:1260`), `replay_rejects_an_unsupported_syscall_by_name` (`hermit-cli/tests/record_replay.rs:1300`) and `record_reports_an_unsupported_syscall_as_a_refusal_not_an_internal_failure` (`hermit-cli/tests/cli.rs:10548`). All three assert the current refusal.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/404: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: unsupported, dispatch unsupported.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore should track a per-thread pending restart: EINTR when none is pending, resume the forwarded call when the kernel armed one, fold restarts into the original call for record/replay); linux_determinism stays conditional; 6 flag behaviours and 6 tests listed; code today fails closed on any use, which the body flags as a hazard for kernel-issued restarts in record mode and non-sequentialized runs (unmeasured; T3, T4); no coverage token, so tracking_status stays todo and ptrace_verification stays untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `restart_syscall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+restart_syscall&body=Discussion+for+the+%60restart_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frestart_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60restart_syscall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: restart_syscall" (https://github.com/rrnewton/hermit/issues/404), legacy variant A1.

Comment history: 2 comment(s) on https://github.com/rrnewton/hermit/issues/404.

### Open questions

- Can a kernel-issued `restart_syscall` reach Detcore in a supported ptrace configuration? The candidates are record mode (`poll` through `handle_external_poll`), `--no-sequentialize-threads`, and `BlockingMode::External` futex waits. The answer depends on how the Reverie ptrace backend handles `ERESTART_RESTARTBLOCK` at syscall exit, and that code is not in this export. T3 and T4 settle it by measurement. If it cannot, the F2-F5 half of the model shrinks to a guard. F1 and F6 still need Detcore's own answer.
- The e2e cell `c-programs/dbt-unsupported-syscall` is disabled on ptrace because the fixture is refused (`tests/e2e/manifests/c-programs.yaml:1198`). When the model lands, the fixture should check the `EINTR` result of its direct call (T1) instead of discarding it, and the cell can then be enabled.

### 2026-07-24 host strace evidence

Note added 2026-10-03: the classification statement in the next paragraph is stale. At hermit f4055eb9cae4, `restart_syscall` is classified Unsupported in `detcore/src/syscall_classification.rs:838`, and by default a run stops at its first use.

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=restart_syscall` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `restart_syscall` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
restart_syscall(<... resuming interrupted tkill ...>) = -1 EINTR (Interrupted system call)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
