---
schema: syscall-pages/v1
syscall: pidfd_send_signal
nr_x86_64: 424
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/320
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pidfd_send_signal&body=Discussion+for+the+%60pidfd_send_signal%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpidfd_send_signal.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pidfd_send_signal%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 15
---

## Summary

`pidfd_send_signal(pidfd, sig, info, flags)` sends a signal to the process a pidfd refers to. Hermit validates the arguments itself (nonzero `flags` gives `EINVAL`, a descriptor it does not model as a pidfd gives `EBADF`) and then forwards the call through record/replay inside the caller's serialized scheduler turn, the same way it forwards `tgkill`.

## Linux semantics

Added in Linux 5.1. The target is fixed when the pidfd is created, so there is no PID-reuse race. The kernel checks the signal number and the caller's permission to signal the target, then queues the signal to the target thread group. With `info == NULL` the kernel fills in a `SI_USER` siginfo carrying the sender's PID and UID. With a caller-supplied siginfo, `si_signo` must equal `sig`, and a sender that is not the target may not claim a kernel-generated `si_code` (`si_code >= 0` or `SI_TKILL` gives `EPERM`). Signal 0 performs only the existence and permission checks. Linux 6.9 added the `flags` values `PIDFD_SIGNAL_THREAD` (1), `PIDFD_SIGNAL_THREAD_GROUP` (2) and `PIDFD_SIGNAL_PROCESS_GROUP` (4) and thread pidfds; earlier kernels require `flags == 0`.

### Flags and options

- F1: `sig > 0` to a live target: the signal is queued to the target thread group and delivered according to the target's mask and handlers.
- F2: `sig == 0`: existence and permission check only; returns 0 for a live target and `ESRCH` after it has exited.
- F3: `info == NULL`: the kernel supplies `si_code = SI_USER`, `si_pid` and `si_uid` of the sender.
- F4: `info` non-NULL: caller-supplied siginfo; `si_signo != sig` gives `EINVAL`, and a forged kernel `si_code` sent to another process gives `EPERM`.
- F5: `flags`: 0 on all kernels; `PIDFD_SIGNAL_THREAD`, `PIDFD_SIGNAL_THREAD_GROUP` and `PIDFD_SIGNAL_PROCESS_GROUP` on Linux 6.9 and later select the signal scope; unknown bits give `EINVAL`.
- F6: Errors from the target and descriptor: a descriptor that is not a pidfd gives `EBADF`; an exited target gives `ESRCH`; an invalid signal number gives `EINVAL`; a pidfd for a process outside the caller's PID namespace hierarchy gives `EINVAL`; a target the caller may not signal gives `EPERM`.
- F7: Descriptor source: pidfds come from `pidfd_open`, from `clone`/`clone3` with `CLONE_PIDFD`, and from duplicating either (`dup`, `fcntl(F_DUPFD)`, `SCM_RIGHTS`, `pidfd_getfd`); all aliases name the same target.

## Determinism analysis

`linux_determinism: nondeterministic`. Which instruction the target is executing when the signal arrives, and the order relative to other signals and to the sender's progress, depend on host scheduling.

### Nondeterminism sources

- The point in the target's execution at which the signal is delivered, and its order relative to other pending signals, is scheduling-dependent.
- `ESRCH` versus success depends on whether the target has exited yet, which is a race with the target's own progress.
- The kernel-supplied `si_pid` and `si_uid` are host identities unless the PID and user namespaces are fixed.

Signal 0 sent to the caller's own pidfd is the one case that repeats without any scheduler: it always returns 0.

## Hermit design

Target policy: `model`. Detcore owns argument validation and orders the send inside the serialized scheduler turn; the kernel performs the queueing under record/replay.

What the code does at hermit f4055eb9cae4:

- Dispatch: a Sysno-guarded arm passes `pidfd` (arg 0) and `flags` (arg 3) to `handle_pidfd_send_signal` (`detcore/src/lib.rs:2052-2065`), because the pinned Reverie exposes the call only as a raw syscall (`detcore/src/lib.rs:2048-2051`). It stays subscribed under the pass-through optimization so the opt-in cannot bypass the handler (`detcore/src/lib.rs:1189-1191`).
- Handler: nonzero `flags` returns `EINVAL` (`detcore/src/syscalls/files.rs:4384-4387`); a descriptor not modeled as `FdType::Pidfd` returns `EBADF` (`detcore/src/syscalls/files.rs:4388-4395`); otherwise the call goes through `record_or_replay` (`detcore/src/syscalls/files.rs:4396`). The doc comment argues that generation runs "inside this thread's serialized scheduler turn, exactly like `tgkill`/`tkill`/`rt_tgsigqueueinfo`" (`detcore/src/syscalls/files.rs:4365-4369`).
- Descriptor model: `pidfd_open` registers its result as `FdType::Pidfd` and records the target (`detcore/src/syscalls/files.rs:4348-4354`); the target is shared by descriptor aliases (unit test `pidfd_target_is_shared_by_descriptor_aliases`, `detcore/src/fd.rs:865-880`).
- Classification: determinized with the rest of the pidfd family (`detcore/src/syscall_classification.rs:571-580`), pinned by the unit test `pidfd_family_is_determinized` (`detcore/src/syscall_classification.rs:2181-2194`).

Why model: the legacy recommendation was NEEDS-HANDLER, and the handler now exists. A pidfd names one process, so the numeric-PID translation that `kill` would need does not arise, and the remaining nondeterminism is the delivery point, which Detcore already owns for every signal it forwards.

Known gaps:

- Nonzero `flags` always returns `EINVAL` (`detcore/src/syscalls/files.rs:4384-4387`), including the `PIDFD_SIGNAL_*` values Linux 6.9 accepts. The comment says "the kernel currently reserves `flags`", which is out of date for newer kernels.
- Only `pidfd_open` registers `FdType::Pidfd` in the files I read (`detcore/src/syscalls/files.rs:4350`); a pidfd from `clone(CLONE_PIDFD)` that Detcore did not register would fail closed with `EBADF`.
- The caller-supplied `info` is forwarded without inspection, so F4 errors come from the kernel; that is deterministic for fixed inputs.
- No guest test calls `pidfd_send_signal`. `hermit-cli/tests/scheduler_deadlock.rs:552-570` uses it on the host side only, to signal a stuck Hermit.

## Recommended tests

- T1 (covers F1, F3): A parent opens a pidfd on a child with `pidfd_open` and sends `SIGUSR1` with `info == NULL`; the child's handler prints `si_code == SI_USER` and `si_pid`; identical output under `run --strict --verify`.
- T2 (covers F2, F6): Signal 0 to the caller's own pidfd returns 0; signal 0 and `SIGUSR1` to a reaped child return `ESRCH`; signal 0 on a regular-file descriptor and on a closed descriptor returns `EBADF`; `sig = 65` returns `EINVAL`.
- T3 (covers F4): `SI_QUEUE` siginfo with a `si_value` to a child is received with that value; `si_code = SI_USER` to another process returns `EPERM`; `si_signo != sig` returns `EINVAL`.
- T4 (covers F5): `flags` 1, 2, 4 and 8 each return `EINVAL` under Hermit, and `flags == 0` works; this pins today's deviation until the `PIDFD_SIGNAL_*` scopes are modeled.
- T5 (covers F7): `dup` of a `pidfd_open` descriptor signals the same child; a `clone(CLONE_PIDFD)` descriptor's result is recorded (success or `EBADF`) and repeats under `--verify`.
- T6 (covers F1, F3): Record and replay T1; replay shows the same delivery point and output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/320#issuecomment-5872907165
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/320: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch sysno-guard.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pidfd_send_signal records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/320 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handler validates flags and FdType::Pidfd, then forwards through record_or_replay inside the serialized turn); linux_determinism stays nondeterministic; 7 flag behaviours and 6 tests listed; no guest test exists and nonzero flags (Linux 6.9 PIDFD_SIGNAL_*) are rejected with EINVAL, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pidfd_send_signal`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pidfd_send_signal&body=Discussion+for+the+%60pidfd_send_signal%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpidfd_send_signal.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pidfd_send_signal%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pidfd_send_signal" (https://github.com/rrnewton/hermit/issues/320), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/320.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pidfd_send_signal`, and by confirming hermit currently routes `pidfd_send_signal` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pidfd_send_signal(3, 0, NULL, 0) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/320#issuecomment-5872907165
