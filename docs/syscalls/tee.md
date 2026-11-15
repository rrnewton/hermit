---
schema: syscall-pages/v1
syscall: tee
nr_x86_64: 276
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/496
  - https://github.com/rrnewton/hermit/issues/517
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tee&body=Discussion+for+the+%60tee%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftee.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tee%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_zero_copy_pipe_syscall"
  refused: true
  refused_only_when_fail_closed: true
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`tee(fd_in, fd_out, len, flags)` duplicates up to `len` bytes from one pipe into another without consuming them from the source. At hermit f4055eb9cae4, an ordinary `hermit run` refuses it with `ENOSYS` (`detcore/src/lib.rs:2138`), and only the `--allow-unsupported-syscalls` opt-out forwards it to the host. The target is to model it: the kernel duplicates the buffers, while Detcore decides when the call may wait.

Manual reference: [tee(2)](https://man7.org/linux/man-pages/man2/tee.2.html).

## Linux semantics

Both descriptors must be pipes, and they must be different pipes. The kernel adds references to the source pipe's buffers into the destination pipe; the source keeps its data, so a later `read` or `splice` on the source still returns it. The return value is the number of bytes duplicated, at most what the source holds and at most the destination's free space. `0` means the source is empty and has no writers.

The call blocks when the source is empty but still has writers, or when the destination is full. `SPLICE_F_NONBLOCK` avoids that, and current kernels treat `O_NONBLOCK` on either descriptor the same way.

Host evidence (Linux 6.17, the measurement host): duplicating one byte between two private pipes returned 1; `tee(5, 5, 1, 0)` returned `EBADF`. The earlier report described that `EBADF` as the result of using one pipe for both ends. Reading of the kernel's order of checks says otherwise: `fd_in` must be open for reading and fd 5 was evidently a write end, so the read-mode check fails before the same-pipe check, which gives `EINVAL`. Both strace lines are under Additional information.

### Flags and options

- F1: `SPLICE_F_NONBLOCK` (0x2): do not block; return `EAGAIN` when the source is empty with writers or the destination is full. `O_NONBLOCK` on either descriptor has the same effect.
- F2: `SPLICE_F_MOVE` (0x1), `SPLICE_F_MORE` (0x4) and `SPLICE_F_GIFT` (0x8): accepted, with no effect on `tee`.
- F3: any flag bit outside `SPLICE_F_ALL` (0xf): `EINVAL`.
- F4: normal duplication: copies references to up to `len` bytes into `fd_out`, leaves the source unchanged, and returns the count, bounded by the source's contents and the destination's free space.
- F5: `len` 0: returns 0 without touching either pipe.
- F6: empty source: with no writers returns 0; with writers blocks (or `EAGAIN`, F1). Full destination: blocks (or `EAGAIN`, F1).
- F7: invalid descriptors: `fd_in` not open for reading or `fd_out` not open for writing gives `EBADF`; a descriptor that is not a pipe, or both descriptors on the same pipe, gives `EINVAL`.
- F8: a destination with no readers gives `EPIPE` and raises `SIGPIPE`. A signal during a blocking wait gives `EINTR`, or the call restarts under `SA_RESTART`.

## Determinism analysis

`linux_determinism: conditional`. The two legacy reports disagreed: https://github.com/rrnewton/hermit/issues/496 said conditional and its duplicate https://github.com/rrnewton/hermit/issues/517 said nondeterministic. Conditional wins: `tee` has no source of nondeterminism of its own. Its result depends only on how full the two pipes are, which depends on when their producers and consumers run. If every process using both pipes is inside the guest and runs in one deterministic schedule, the count and the blocking point repeat. They do not repeat when a pipe's other end belongs to a host process (for example a guest whose stdin is a host pipe), or when the schedule is not controlled.

### Nondeterminism sources

- Fill level of the source pipe and free space of the destination pipe, which depend on producer and consumer timing (F4, F6). The pipe capacity itself is host-configurable; Detcore pins it, see Hermit design.
- Whether the descriptors are nonblocking (F1).
- When a signal arrives during a blocking wait (F8).
- A pipe end held by a process outside the guest.

## Hermit design

Target policy: `model`, for the same reasons as `splice` (https://github.com/rrnewton/hermit/issues/478), which shares the dispatch arm. The kernel duplicates the buffers; Detcore decides when the call may block.

Target (owner decisions 9, 14 and 6): `model`, confirmed. Owner decision 9 (2026-10-04) names `tee` with `splice` for modelling. Today an ordinary run refuses the call with a fixed `ENOSYS` (`detcore/src/lib.rs:2138`-`2144`; the fail-closed default is documented at `detcore-model/src/config.rs:447`), and `--allow-unsupported-syscalls` forwards it to the host unchanged (`detcore/src/lib.rs:367`).

- Blocking (owner decision 14): Detcore never lets the call wait in the kernel. It runs the call as a probe with `SPLICE_F_NONBLOCK` added. If the probe returns `EAGAIN` and the guest did not ask for nonblocking behaviour (F1), the thread parks in the deterministic scheduler and probes again when either pipe changes, or when a signal arrives (`EINTR`, F8). Both ends of a `tee` are pipes, so Detcore can observe the wait only when both are guest pipes; a `tee` with an end shared with a host process keeps today's `ENOSYS` until a wait for that end exists.
- Errno order (owner decision 6): the kernel performs every check, so Detcore's pre-checks (both descriptors guest pipes) may only choose the route and must never return an errno of their own. Linux's order differs from `splice` (`fs/splice.c`, `SYSCALL_DEFINE4(tee)`, `do_tee`; measured natively on the measurement host, Linux 7.1.3, 2026-10-04): flag bits outside `SPLICE_F_ALL` give `EINVAL` first, even with `len` 0 (F3); then `len` 0 returns 0 (F5), even with a closed descriptor or a write end as `fd_in`; then a descriptor that is not open gives `EBADF`; then `fd_in` not open for reading or `fd_out` not open for writing gives `EBADF` (F7); only then a descriptor that is not a pipe, or both on the same pipe, gives `EINVAL` (F7), so a write end as `fd_in` with a regular file as `fd_out` gives `EBADF`. T8 checks the pairs where two errors compete.

What the code does at hermit f4055eb9cae4:

- `tee` is classified `Determinized` (`detcore/src/syscall_classification.rs:497`-`505`) and belongs to `is_zero_copy_pipe_syscall` (`detcore/src/syscall_classification.rs:1224`).
- The dispatch arm (`detcore/src/lib.rs:2138`-`2144`) returns `ENOSYS` when `panic_on_unsupported_syscalls` is set, which an ordinary `hermit run` does (`detcore-model/src/config.rs:447`), and otherwise forwards through `passthrough` (`detcore/src/lib.rs:367`). The family is in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), hence `refused_only_when_fail_closed: true` in `code_facts`.
- `hermit-cli/tests/zero_copy_pipe_fallback.rs:34` checks both states on ptrace with `tests/c/tee_enosys.c`: `ENOSYS` under `run --verify`, and under the opt-out a working `tee` whose byte is then readable from both pipes.

Why the current state is not the target:

- `ENOSYS` is not an outcome Linux produces for `tee`. Unlike `splice`, `tee` has no simple `read`/`write` fallback: a program that needs the non-consuming copy has to restructure its data flow, so a refusal is more likely to break the caller.
- Under the opt-out, every guest pipe is physically `O_NONBLOCK` when threads are sequentialized (`detcore/src/syscalls/files.rs:3584`-`3586`), hidden from the guest's `F_GETFL` (comment at `detcore/src/syscalls/files.rs:3582`). Since the kernel turns `O_NONBLOCK` on either descriptor into `SPLICE_F_NONBLOCK` (F1), a guest's blocking `tee` on an empty guest pipe should return `EAGAIN` instead of waiting. https://github.com/rrnewton/hermit/issues/3677 records this defect. It was found by reading the code at c9f58b2de6eb and has not been measured; T3 measures it. The cause is wider than the opt-out: the pipes are made `O_NONBLOCK` whenever threads are sequentialized, and the flag only opens the forwarding route to the bug.

How to realize `model`:

- Run `tee` through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`): nonblocking, retried under `InternalIOPolling` (`detcore/src/syscalls/helpers.rs:254`-`267`). The guest-pipe test there looks at one descriptor (`syscall_targets_internal_fd`, `detcore/src/syscalls/helpers.rs:797`; `get_fd`, `detcore/src/syscalls/helpers.rs:815`); `tee` needs both to be guest pipes for the retry, and otherwise is blocking external I/O.
- Return `EAGAIN` only when the guest asked for it, with `SPLICE_F_NONBLOCK` or `O_NONBLOCK` in its own view of either descriptor.
- Counts follow the pinned 8 KiB pipe capacity (`detcore/src/syscalls/files.rs:256`, applied at `detcore/src/syscalls/files.rs:3612`).
- Record/replay: `tee` moves no data through guest memory, so replay must reproduce the destination pipe's contents. Between guest pipes, re-executing the call reproduces them, given the source was reproduced; the guest-pipe path already routes reads and writes through the recorder (`detcore/src/syscalls/helpers.rs:262`-`266`). A source pipe fed by a host process needs its bytes recorded.

Why not the other policies:

- `refuse` (today's default) breaks callers with no fallback, as above.
- `passthrough` lets a wait happen in the kernel or turns it into a spurious `EAGAIN`.
- `emulate` would need a non-consuming read of a pipe, which Linux offers only through `tee` itself.

## Recommended tests

- T1 (covers F4, F5): under `hermit run --verify` on ptrace, `tee` one byte and then a multi-KiB block between two guest pipes; read both pipes and check the data appears in each; `len` 0 returns 0 and changes neither pipe.
- T2 (covers F6): `tee` from an empty source with no writers returns 0; from an empty source whose writer is another guest thread, the call waits until that thread writes, at the same point on every run; a full destination waits for its reader.
- T3 (covers F1): `SPLICE_F_NONBLOCK` with an empty source gives `EAGAIN`; a blocking `tee` on guest pipes the guest never made nonblocking must wait and must not return `EAGAIN`. Run the second case also under `--allow-unsupported-syscalls` as the measurement https://github.com/rrnewton/hermit/issues/3677 lacks; that run records today's opt-out behaviour and is not a pass condition for `model`.
- T4 (covers F2, F3): `SPLICE_F_MOVE`, `SPLICE_F_MORE` and `SPLICE_F_GIFT` are accepted and change no result; flag `0x10` gives `EINVAL`.
- T5 (covers F7): `EBADF` for a write end as `fd_in` (the case recorded in the host strace); `EINVAL` for both ends on the same pipe opened read-write through a FIFO, and for a regular file in either position.
- T6 (covers F8): `EPIPE` and `SIGPIPE` for a destination with no readers; a signal with a handler interrupts a waiting `tee` with `EINTR`.
- T7 (covers none): regression check of today's behaviour. Existing: `hermit-cli/tests/zero_copy_pipe_fallback.rs:34` and the e2e cell `c-programs/tee-enosys` (`tests/e2e/manifests/c-programs.yaml:9971`, verify with `ptrace: true` at line 9990), both on `tests/c/tee_enosys.c`. They test the refusal, not the target, so they cover no flag; their `ENOSYS` expectation has to change when `model` lands.
- T8 (covers F3, F5, F7): errno order (owner decision 6). Run each case natively first and require Hermit's result to equal it: `len` 0 with flag `0x10` gives `EINVAL`; `len` 0 with a write end as `fd_in` returns 0; a write end as `fd_in` with a regular file as `fd_out` gives `EBADF`, not `EINVAL`; a read-only regular file as `fd_in` gives `EINVAL`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/496: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_zero_copy_pipe_syscall, refused only when fail-closed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/517 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel duplicates the buffers, Detcore owns blocking via the guest-pipe nonblocking retry path; today ordinary runs refuse with ENOSYS); linux_determinism stays conditional over the duplicate's nondeterministic; merged https://github.com/rrnewton/hermit/issues/517; corrected the reading of the tee(5, 5) EBADF probe; 8 flag behaviours, 7 tests; existing tests check only the ENOSYS refusal, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 14 and 6 applied (decision 9 names tee for modelling; new `Target (owner decisions 9, 14 and 6)` paragraph with the probe-and-park wait and Linux's check order, which puts the flag check before the len-0 check, unlike splice; confirmed natively on Linux 7.1.3); target_policy model -> model unchanged; new T8 (errno order); the opt-out EAGAIN hypothesis now cites https://github.com/rrnewton/hermit/issues/3677 as code-read and unmeasured, T3 wording updated; removed the open question (keep ENOSYS or model), answered by decision 9
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T7 covered by tests/c/tee_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `tee`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tee&body=Discussion+for+the+%60tee%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftee.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tee%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: tee" (https://github.com/rrnewton/hermit/issues/496), legacy variant A1. Its duplicate https://github.com/rrnewton/hermit/issues/517 was filed the same day with the same title (legacy variant A1, batch 5 of the classification sweep) and is merged into the sections above. The comment histories stay on the two issues; https://github.com/rrnewton/hermit/issues/517 had 0 comments at retirement.

Merge notes:

- Determinism: https://github.com/rrnewton/hermit/issues/496 said conditional, https://github.com/rrnewton/hermit/issues/517 said NONDETERMINISTIC. Conditional was kept; the reason is under Determinism analysis.
- The first report said "an invalid (same-pipe) call fails EBADF". The duplicate's probe and the kernel's check order (read mode on `fd_in` first, same pipe later) point to `EBADF` coming from the write end used as `fd_in`; a same-pipe call on a readable and writable descriptor gives `EINVAL`. The correction is under Linux semantics and F7; T5 checks both.
- Recommended action: both said NEEDS-HANDLER. This body chooses `model`.
- Both reports said Hermit routed `tee` through the UNCLASSIFIED temporary-passthrough block of `detcore/src/syscall_classification.rs` (at `dd60278f` for the duplicate). That is stale at f4055eb9cae4, where the call is refused by default.
- The duplicate's source list (pipe fill levels, producer timing, signal interruption, nonblocking flags, consumer activity) is kept under Nondeterminism sources.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/496)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=tee` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs.

Observed strace line:
```
tee(5, 5, 1, 0)                 = -1 EBADF (Bad file descriptor)
```

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/517)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=tee`. The harness exited 0. The probe duplicated one byte between private pipes.

Observed strace line:
```text
tee(3, 6, 1, 0)                 = 1
```

Both reports were produced by the autonomous syscall-classification sweep (ranks 151-250, and batch 5 covering ranks 201-250, of the then-UNCLASSIFIED list), from host testing and manual research, with no code change proposed.
