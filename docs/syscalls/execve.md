---
schema: syscall-pages/v1
syscall: execve
nr_x86_64: 59
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+execve&body=Discussion+for+the+%60execve%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexecve.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60execve%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 10
---

## Summary

`execve(2)` replaces the calling process's program image with a new one loaded from a file, keeping the PID and most process attributes. The kernel performs the load. Detcore updates its own per-process model around it: descriptor table, address-space identity, robust list, `clear_child_tid`, POSIX timers and scheduler identity. It also overwrites the kernel's random `AT_RANDOM` bytes with seeded ones, and rolls everything back if the call fails.

## Linux semantics

On success the call does not return. The kernel discards the old address space and maps the new executable (or the interpreter named by a `#!` line or a `binfmt_misc` rule), builds a new stack holding `argv`, `envp` and the auxiliary vector, and starts the program at its entry point. The PID, parent, process group, session, credentials (unless set-user-ID or set-group-ID applies), resource limits, current directory, signal mask, pending signals and descriptors not marked close-on-exec all survive. On failure the old image is untouched and the call returns an error. Errors that matter here: `ENOENT`, `EACCES`, `ENOEXEC`, `E2BIG`, `EFAULT`, `ELOOP`, `ETXTBSY`, `ENAMETOOLONG` and `ENOMEM`.

### Flags and options

- F1: success: the image is replaced, `argv` and `envp` are copied to the new stack, and the call does not return.
- F2: descriptors marked `FD_CLOEXEC` are closed; all others stay open, sharing their open file descriptions.
- F3: failure (`ENOENT`, `EACCES`, `ENOEXEC`, `E2BIG`, `EFAULT`, `ELOOP`, `ETXTBSY`, `ENAMETOOLONG`, `ENOMEM`): the call returns -1 and the old image continues unchanged.
- F4: interpreter scripts: a `#!` line, or a `binfmt_misc` rule, runs the named interpreter with the script path prepended to the arguments.
- F5: set-user-ID and set-group-ID files and file capabilities change credentials and set `AT_SECURE`, unless `no_new_privs` or a `nosuid` mount suppresses them.
- F6: exec from a thread that is not the thread-group leader: every other thread is killed, and the caller takes over the leader's PID and TID.
- F7: a `vfork` child that execs releases its parent, which resumes.
- F8: signals: caught signals are reset to `SIG_DFL`; ignored signals stay ignored; the mask and pending signals are kept; the alternate signal stack is cleared.
- F9: timers: POSIX timers (`timer_create`) are deleted; `alarm` and `setitimer` timers survive.
- F10: the new image's startup state: 16 random bytes at `AT_RANDOM`, and the stack, `mmap` base and vDSO placed by address-space layout randomization.
- F11: the robust futex list and the `clear_child_tid` registration are cleared; memory mappings, locks and attached shared memory are dropped.

## Determinism analysis

Given the same file system contents, arguments, environment and credentials, the result of an exec is mostly fixed: whether it fails, with which error, and which descriptors and signal dispositions survive. Two things differ on every run. `AT_RANDOM` holds 16 fresh random bytes, which glibc uses for the stack protector canary and pointer guard. With layout randomization enabled, the stack, heap, `mmap` base and vDSO addresses move. A non-leader exec also races with the threads it kills, so what those threads finished before dying depends on timing. `linux_determinism` is `conditional`: deterministic for a fixed file system and inputs, apart from the startup randomness and the threads that a non-leader exec kills.

## Hermit design

`target_policy` is `model`: the kernel performs the load, and Detcore keeps its model of the process consistent with it, replacing the random startup bytes with seeded ones. Code at hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2746`, which converts the call to `execveat` and runs `handle_execveat` at `detcore/src/syscalls/threads.rs:1443-1535`.
- F2: `threads.rs:1459-1467` builds the post-exec descriptor table with `for_exec` and lists the open file descriptions that close-on-exec will drop (`open_files_closed_on_exec`). `threads.rs:1482-1487` releases their scheduler ports.
- `threads.rs:1471-1480` calls `prepare_exec` (`detcore/src/tool_global.rs:3169`), which tells the global scheduler that this process is about to exec.
- F11: `threads.rs:1489-1499` installs the new descriptor table and an empty memory model, derives the new address-space identity with `for_exec`, and takes the robust-list head, because Linux clears it on exec.
- F1: `threads.rs:1502` runs the call through `record_or_replay`. It returns only on failure, so the result is unwrapped as an error.
- F3: on failure, `threads.rs:1504-1516` restores the old descriptor table, memory model, address-space identity and robust list, calls `cancel_exec`, and restores the released ports; `threads.rs:1534` returns the kernel's errno.
- After a successful exec, the backend calls `handle_post_exec` (`detcore/src/lib.rs:1735-1805`):
  - F6: `reconnect_exec` (`detcore/src/tool_global/exec_identity.rs:190-243`) rebinds Detcore's thread state to the new TID after a non-leader exec, keeping the thread's logical time. It returns `EOPNOTSUPP` when preemptions are being recorded or replayed (`exec_identity.rs:221-226`). `lib.rs:1738-1750` then stops the run with `HERMIT_POLICY_REFUSAL_EXIT`. Ordinary runs continue.
  - F9: `lib.rs:1755-1765` deletes the old image's POSIX timer IDs; `alarm` and `ITIMER_REAL` survive, as the comment says.
  - F11: `lib.rs:1766-1770` resets the modeled `clear_child_tid` address to 0.
  - F10: `lib.rs:1775-1794` overwrites the 16 `AT_RANDOM` bytes from the thread's seeded generator (`random::initialize_auxv`), unless an earlier write already filled them for this image.
  - `lib.rs:1799-1801` lets the record/replay layer commit its pending exec state.
- F4, F5, F7 and F8 are carried out by the kernel; Detcore has no exec-specific code for them in the handler above.
- Target (owner decision 10): no panic that the guest's exec can reach. Two exist on this path, found by reading the code, not measured:
  - `threads.rs:1502` unwraps the result as an error. A plain `execve` never returns 0, so only `execveat` with `AT_EXECVE_CHECK` reaches this; the `execveat` body refuses that flag with `ENOSYS` before `prepare_exec`.
  - Under `hermit record`, the recorder's `handle_exec` (`hermit-cli/src/recorder.rs:924`) panics at `recorder.rs:950-953` when the first exec of the root thread fails with `ENOEXEC` (F3), that is, when the program Hermit is asked to run is a script without a `#!` line. Natively, glibc's `execvpe` retries such a file with `/bin/sh`. The target records the `ENOEXEC` and returns it (`recorder.rs:954-955` already does this for every other failure), so the fallback exec runs and is recorded like any other; until the recorder can represent that second exec, Hermit stops with its policy-refusal exit status and the existing message, not a panic. `recorder.rs:949` (`expect_err` on a successful return) is the recorder's copy of the `threads.rs:1502` unwrap and is closed by the same `execveat` refusal.

## Recommended tests

- T1 (covers F1, F7): a `vfork` child execs the same program with new arguments, prints, and exits; the parent resumes and waits; output and PIDs must match across runs under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/vforkExec.c` (cell at `tests/e2e/manifests/c-programs.yaml:10676`, verify ci on ptrace).
- T2 (covers F1, F2): `fork`, then `execve("/bin/echo", ...)` in the child with its stdout on a pipe; the parent reads the bytes and the exit status.
  Existing candidate: `tests/c/fork_exec_pipeline.c` (cell at `tests/e2e/manifests/c-programs.yaml:17118`, verify ci on ptrace).
- T3 (covers F2): a descriptor marked close-on-exec is closed after exec, and its number is reused by the next open, while an inherited descriptor survives.
  Existing candidate: `tests/c/record_replay_fd_close.c` (cell at `tests/e2e/manifests/c-programs.yaml:6924`, verify ci on ptrace), whose `--after-exec` phase checks the reused slot.
- T4 (covers F3): `execve` of a missing path returns `ENOENT`, and the program keeps running normally, including a later sleep.
  Existing candidate: `tests/c/dbt_exec_failure.c` (cell at `tests/e2e/manifests/c-programs.yaml:797`, verify ci on ptrace). Extend it with `EACCES` (no execute bit), `ENOEXEC` (a file with no valid header), `E2BIG` and `EFAULT` (an unmapped `argv`).
- T5 (covers F4): exec a `#!/bin/sh` script and a script whose interpreter is missing (`ENOENT`); output must match across runs.
- T6 (covers F5): exec a set-user-ID helper under `no_new_privs`, and check that `AT_SECURE` and the effective UID are the same in every run.
- T7 (covers F6): a non-leader thread execs while the leader and another thread run; the new image sees the leader's PID, and its time continues.
  Existing candidates: `run_ptrace_nonleader_exec_preserves_identity_and_time` at `hermit-cli/tests/cli.rs:5520`, `run_ptrace_nonleader_exec_displaces_runnable_leader` at `hermit-cli/tests/cli.rs:5535`, both driving `hermit-cli/tests/fixtures/nonleader_exec.c`. `cli.rs` also runs other backends, so it cannot serve as a coverage path.
- T8 (covers F8): before exec, install a handler for `SIGUSR1`, ignore `SIGUSR2`, block `SIGTERM` and leave it pending; after exec, `SIGUSR1` is `SIG_DFL`, `SIGUSR2` still ignored, and `SIGTERM` still blocked and pending.
- T9 (covers F9): an `alarm` set before exec fires after exec, while a `timer_create` timer does not; the virtual clock never goes backwards across the exec.
  Existing candidate: `tests/c/clock_exec_continuity.c` (cell at `tests/e2e/manifests/system-utils.yaml:1724`, verify ci on ptrace) checks the clock across `execv`, not the timers.
- T10 (covers F10): print the `AT_RANDOM` bytes and the raw vDSO and stack addresses after exec, and require two runs to match.
  Existing candidate: `tests/c/startup_surface_identity.c` (cell at `tests/e2e/manifests/system-utils.yaml:1528`, verify ci on ptrace) checks the startup state of the first image only.
- T11 (covers F11): a thread registers a robust list and a `clear_child_tid` address, then the process execs; the new image's `get_robust_list` returns no list, and a later thread exit wakes nothing stale.
- T12 (covers F3): under `hermit record` and then `hermit replay`, run a shell script that has no `#!` line as the program itself. Assert that Hermit does not panic: either the script's output matches a native run and replay reproduces it, or, under the interim, Hermit exits with its policy-refusal status and prints the shebang message, with no Rust panic text on stderr. Natively the script runs through `/bin/sh`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel loads the image; Detcore updates descriptor, memory, robust-list, clear_child_tid, timer and exec identity state, seeds AT_RANDOM, rolls back on failure); linux_determinism stays conditional; 11 flag behaviours and 11 tests listed; open question on the non-leader exec refusal under preemption record/replay and on where layout randomization is disabled; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 10 applied (guest-reachable panics on the exec path named: threads.rs:1502 and recorder.rs:949, closed by the execveat AT_EXECVE_CHECK refusal; recorder.rs:950-953 initial-exec ENOEXEC panic, target records ENOEXEC so execvpe falls back); target_policy stays model; new T12; no open question removed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/vforkExec.c, T2 covered by tests/c/fork_exec_pipeline.c, T3 covered by tests/c/record_replay_fd_close.c, T4 covered by tests/c/dbt_exec_failure.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `execve`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+execve&body=Discussion+for+the+%60execve%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexecve.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60execve%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `execve`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`execveat` shares the handler (`detcore/src/lib.rs:2747`); its own body covers the directory-relative and `AT_EMPTY_PATH` cases.

On KVM, a non-leader exec that returns `ENOSYS` is refused through `refuse_unserviceable_operation` (`detcore/src/syscalls/threads.rs:1521-1532`). This is not ptrace behaviour and is recorded only for completeness.

### Open questions

- A non-leader exec stops the whole run while preemptions are being recorded or replayed (`detcore/src/tool_global/exec_identity.rs:221-226`, `detcore/src/lib.rs:1738-1750`). Is this an accepted limit, or should preemption histories carry an exec incarnation so the case can be supported?
- `tests/c/startup_surface_identity.c` requires raw vDSO and stack addresses to match across two runs, which needs layout randomization off. No code in the hermit tree at this SHA disables it for the ptrace backend (the only `ADDR_NO_RANDOMIZE` is `hermit-cli/src/sabre_ptrace.rs:1306`, for SaBRe). The mechanism is presumably in the Reverie ptrace backend, which this review did not examine. Confirm that it applies again after every exec, not only to the first image.
