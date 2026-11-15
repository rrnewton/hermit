---
schema: syscall-pages/v1
syscall: pidfd_open
nr_x86_64: 434
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/319
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pidfd_open&body=Discussion+for+the+%60pidfd_open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpidfd_open.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pidfd_open%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 24
---

## Summary

Returns a file descriptor that refers to a process (by PID), usable for polling exit and sending signals. Hermit models it: Detcore checks the flags itself, lets the kernel create the pidfd (recorded in record mode), and registers the result in its descriptor table as a pidfd naming the target process, so that `pidfd_send_signal`, `pidfd_getfd` and `waitid(P_PIDFD)` can use it.

Manual reference: [pidfd_open(2)](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).

## Linux semantics

Added in Linux 5.3. Pollable for process exit. Verified: opening the caller's own pid returns an fd deterministically.

The raw signature is `pidfd_open(pid, flags)`; glibc has no wrapper before 2.36, so programs often issue it with `syscall()`. `pid` is interpreted in the caller's PID namespace. The kernel checks `flags` first, then looks up the process. The new descriptor always has close-on-exec set and takes the lowest free number. It holds a reference to the process, not to the number, so it keeps naming the same process even after the number is reused. It becomes readable (`POLLIN`) when the process exits, and a pidfd can be opened on a zombie that has not been reaped.

### Flags and options

- F1: `pid`: a positive process id in the caller's PID namespace. Zero or negative gives `EINVAL`; no such process gives `ESRCH`. The id of a thread that is not a thread-group leader gives `EINVAL` unless F4 is set.
- F2: `flags` = 0: a blocking pidfd. `waitid(P_PIDFD)` on it waits for the process to exit.
- F3: `PIDFD_NONBLOCK` (equal to `O_NONBLOCK`, Linux 5.10): a non-blocking pidfd. `waitid(P_PIDFD)` without `WNOHANG` returns `EAGAIN` while the process is still running.
- F4: `PIDFD_THREAD` (equal to `O_EXCL`, Linux 6.9): the pidfd names a thread; `pid` may be any thread id, and the pidfd becomes readable when that thread exits.
- F5: any other flag bit gives `EINVAL`.
- F6: the result: the lowest free descriptor number, always close-on-exec; `/proc/self/fdinfo/<fd>` shows the target's `Pid:` and `NSpid:`.
- F7: poll and read readiness: `POLLIN` once the target has exited, whether or not it has been reaped; never ready while it runs.
- F8: resource errors: `EMFILE` (`RLIMIT_NOFILE`), `ENFILE`, `ENOMEM`, and `ENODEV` if the kernel cannot create the pidfd file.

## Determinism analysis

`linux_determinism: conditional`. The operation itself is deterministic given its inputs. Its inputs and outputs are numbers from two shared allocators: the PID namespace and the descriptor table. In the host PID namespace, the pid a program opens depends on every other process on the machine, and whether the target is still alive depends on scheduling. The descriptor number repeats whenever the caller's descriptor table is in the same state. F7 readiness depends on when the target exits relative to the poll, which is a scheduling question. F8 depends on host limits.

The call repeats when the PID namespace is private and its processes are created in a fixed order, the descriptor table evolves the same way, and a deterministic scheduler orders the target's exit against the caller.

### Nondeterminism sources

The PID input and returned fd must be virtualized to hermit's deterministic namespaces; otherwise host pid/fd numbers leak.

- Whether the target is alive, a zombie or gone at the time of the call (F1, F7), which is decided by scheduling
- Host limits for F8
- The kernel version: F3 needs 5.10 and F4 needs 6.9

## Hermit design

Target policy: `model`. The kernel creates the pidfd, because only the kernel can hold a reference to a process. Detcore decides which flags are accepted, records the result, and keeps its own model of the descriptor and the process it names, which later pidfd calls consult.

What the code does at f4055eb9:

- The dispatch arm (`detcore/src/lib.rs:2591`, under the tag `TODO-HUMAN-REVIEW(PR-862)`, which is https://github.com/rrnewton/hermit/pull/862) calls `handle_pidfd_open` (`detcore/src/syscalls/files.rs:4338`).
- Only `O_NONBLOCK` is allowed (`detcore/src/syscalls/files.rs:4343`); any other bit gives `EINVAL` before the kernel sees the call (`detcore/src/syscalls/files.rs:4344`-`4345`). That covers F5, and it also rejects F4 `PIDFD_THREAD`, which Linux 6.9 and later accept. See Open questions.
- The call goes through `record_or_replay` (`detcore/src/syscalls/files.rs:4348`), so in record mode the result is recorded and in replay mode it is returned from the recording.
- The new descriptor is added to Detcore's table as `FdType::Pidfd` with `O_CLOEXEC` and the guest's `O_NONBLOCK` bit (`detcore/src/syscalls/files.rs:4349`-`4350`), and its target is stored as `DetPid::from_raw(pid)` (`detcore/src/syscalls/files.rs:4351`-`4354`). The target is kept in the open-file record that descriptor aliases share (`detcore/src/fd.rs:91`-`97`, setter at line 343).
- The guest's pid is passed to the kernel unchanged. That repeats because the guest runs in its own PID namespace (`default_container`, `hermit-cli/src/bin/hermit/container.rs:442`), where creation order is fixed by the scheduler; `detcore/src/syscalls/signal.rs:674` relies on the same property for signal targets.
- Later calls use the model. `pidfd_send_signal` refuses any descriptor that is not a registered `FdType::Pidfd` with `EBADF` (`detcore/src/syscalls/files.rs:4388`-`4395`). `waitid(P_PIDFD)` reads the descriptor's `O_NONBLOCK` status and returns `EAGAIN` for a non-blocking pidfd (F3); a blocking `waitid` on a blocking pidfd returns `EOPNOTSUPP` (`detcore/src/syscalls/threads.rs:1824`-`1841`), because Detcore cannot yet keep the pidfd's identity while it waits.
- Classification: `Determinized` (`detcore/src/syscall_classification.rs:565`-`569`), with unit test `pidfd_family_is_determinized` (`detcore/src/syscall_classification.rs:2182`). It is also in the record/replay subscription (`detcore/src/lib.rs:1187`).

Why `model`: `emulate` is not possible, because the descriptor must be a real kernel object for `poll` and for the kernel side of `waitid`. `passthrough` would leave the descriptor unknown to Detcore, and `pidfd_send_signal` would then fail with `EBADF`. `refuse` would break programs such as service managers and process supervisors that use pidfds.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **F4 `PIDFD_THREAD`** is refused with `EINVAL`, which matches a kernel older than 6.9 but not the host kernel. See Open questions.
- **F2 blocking `waitid`** returns `EOPNOTSUPP`, which Linux never returns there. A program that opens a blocking pidfd and waits on it fails under Hermit.
- **F7 readiness** is decided by the kernel when the guest polls; no Detcore code for polling pidfds was found at this SHA, so whether a poll that races the target's exit is ordered by the scheduler has not been checked.

## Recommended tests

- T1 (covers F1, F2, F6): open a pidfd on the caller's own pid with flags 0, check close-on-exec, then open a second one and check the two descriptors differ; the output must match across `--verify` runs.
  Existing candidates: `tests/c/pidfd_open_self.c:18` (cell `c-programs/pidfd-open-self`, program at `tests/e2e/manifests/c-programs.yaml:5113`, verify ci on ptrace) and `tests/c/pidfd_open_self_pair.c:29` (cell `c-programs/pidfd-open-self-pair`, program at `tests/e2e/manifests/c-programs.yaml:15581`, verify ci on ptrace).
- T2 (covers F5): flags `1` and `0xFFFFFFFF` return `EINVAL`.
  Existing candidates: `tests/c/pidfd_open_self.c:33` and `tests/c/pidfd_open_self_pair.c:44`, in the same cells as T1.
- T3 (covers F3): open a child's pidfd with `O_NONBLOCK`; `waitid(P_PIDFD)` returns `EAGAIN` while the child runs, then the exit status after it exits.
  Existing candidates: `tests/c/pidfd_waitid_child.c:44` (cell `c-programs/pidfd-waitid-child`, program at `tests/e2e/manifests/c-programs.yaml:5229`, verify ci on ptrace), and `waitid_polls_until_child_exit_and_supports_wnohang` (`detcore/tests/misc/mod.rs:1043`; the `O_NONBLOCK` open is at line 1102 and the blocking open at line 1123).
- T4 (covers F7): poll a live self pidfd and find it not ready; poll a child's pidfd until the child exits and find it ready, both before and after the child is reaped.
  Existing candidate for the first half: `tests/c/pidfd_poll_self.c:18` (cell `c-programs/pidfd-poll-self`, program at `tests/e2e/manifests/c-programs.yaml:5171`, verify ci on ptrace).
- T5 (covers F1, F8): pid 0 and pid -1 return `EINVAL`; a pid with no process returns `ESRCH`; a non-leader thread id with flags 0 returns `EINVAL`; with `RLIMIT_NOFILE` lowered to the current count the call returns `EMFILE`.
- T6 (covers F4): `PIDFD_THREAD` on a thread id. Today Hermit returns `EINVAL`; the expected result depends on the open question below.
- T7 (covers F2, F6): record a program that opens a pidfd on a child and sends it a signal through `pidfd_send_signal`, then replay it. Replay must return the same descriptor number and the signal must succeed, which shows the descriptor was registered in replay as well.
  Related: `replay_reissues_pidfd_getfd_success_and_failure` (`hermit-cli/tests/flock_exclusion.rs:849`) records and replays a scenario that opens a self pidfd at `tests/c/flock_exclusion.c:891`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/319#issuecomment-5872904456
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/319: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pidfd_open records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/319 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (flags checked in Detcore, kernel pidfd recorded and registered as FdType::Pidfd with its target, detcore/src/syscalls/files.rs:4338); linux_determinism conditional; 8 flags, 7 tests; open question: should Hermit accept PIDFD_THREAD (refused with EINVAL at files.rs:4343-4345); no coverage recorded, so todo.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pidfd_open`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pidfd_open&body=Discussion+for+the+%60pidfd_open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpidfd_open.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pidfd_open%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pidfd_open" (https://github.com/rrnewton/hermit/issues/319), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/319.

### Open questions

- **Should Hermit accept `PIDFD_THREAD`?** Linux 6.9 added `PIDFD_THREAD` (`O_EXCL`), and the host kernel accepts it. Detcore allows only `O_NONBLOCK` (`detcore/src/syscalls/files.rs:4343`-`4345`), so a guest gets `EINVAL`, as from a kernel older than 6.9. Accepting it would also mean teaching `pidfd_send_signal` and `waitid` about thread pidfds. The owner should decide whether Hermit presents a pre-6.9 pidfd interface on purpose, or should support thread pidfds. T6 waits on this answer.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pidfd_open`, and by confirming hermit currently routes `pidfd_open` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pidfd_open(1549764, 0)          = 3
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/319#issuecomment-5872904456
