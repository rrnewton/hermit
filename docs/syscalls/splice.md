---
schema: syscall-pages/v1
syscall: splice
nr_x86_64: 275
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/478
  - https://github.com/rrnewton/hermit/issues/482
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+splice&body=Discussion+for+the+%60splice%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsplice.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60splice%60%2C+or+in+what+the+page+says%3F%0A
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

`splice(fd_in, off_in, fd_out, off_out, len, flags)` moves up to `len` bytes between two descriptors inside the kernel, without a copy through user memory; at least one of the two must be a pipe. At hermit f4055eb9cae4, an ordinary `hermit run` refuses it with `ENOSYS` (`detcore/src/lib.rs:2138`), and only the `--allow-unsupported-syscalls` opt-out forwards it to the host. The target is to model it: the kernel moves the bytes, while Detcore decides when the call may wait.

Manual reference: [splice(2)](https://man7.org/linux/man-pages/man2/splice.2.html).

## Linux semantics

Three directions exist: pipe to file or socket, file or socket to pipe, and pipe to pipe. The return value is the number of bytes moved. It can be shorter than `len`: a pipe source gives at most what it holds, a pipe destination takes at most its free space, and a file source stops at end of file. `0` means end of input: the source is a pipe with no data and no writers, or a file at its end.

Offsets apply only to the end that is not a pipe. With `off_in`/`off_out` NULL, the file position is used and advanced. With a pointer, the kernel reads the offset from it, uses it, writes the updated offset back, and leaves the file position alone. Passing an offset pointer for a pipe end is `ESPIPE`.

When the source is a regular file, the pipe receives references to page-cache pages, not a copy. A write to the file after the splice, but before a reader drains the pipe, can change what that reader sees.

Pipe operations can block: an empty source pipe that still has writers, or a full destination pipe. Blocking is avoided with `SPLICE_F_NONBLOCK`. Current kernels also treat `O_NONBLOCK` on a pipe-side descriptor as `SPLICE_F_NONBLOCK`. A non-pipe end can still block on its own (for example a socket with no data), whatever the flag says.

Host evidence (Linux 6.17, the measurement host): splicing 4 bytes from `/etc/hostname` into a pipe returned 3, a short count bounded by the file's end, and moving one byte between two private pipes returned 1. Both strace lines are under Additional information.

### Flags and options

- F1: `SPLICE_F_MOVE` (0x1): a hint to move pages instead of copying. It has been a no-op since Linux 2.6.21, but is still accepted.
- F2: `SPLICE_F_NONBLOCK` (0x2): do not block on the pipe side; return `EAGAIN` instead. `O_NONBLOCK` on a pipe-side descriptor has the same effect. A non-pipe end can still block.
- F3: `SPLICE_F_MORE` (0x4): more data will follow. When the destination is a socket, it is passed on like `MSG_MORE`.
- F4: `SPLICE_F_GIFT` (0x8): meaningful only for `vmsplice`; accepted by `splice` with no effect.
- F5: any flag bit outside `SPLICE_F_ALL` (0xf): `EINVAL`.
- F6: offsets: `off_in`/`off_out` NULL uses and advances the file position; a pointer is read, used and updated, and the file position does not move. A pointer for a pipe end gives `ESPIPE`, a bad pointer `EFAULT`, a negative offset `EINVAL`.
- F7: pipe to file or socket (`fd_in` a pipe): moves at most the bytes the pipe holds. An empty pipe with no writers returns 0; with writers it blocks (or `EAGAIN`, F2).
- F8: file or socket to pipe (`fd_out` a pipe): moves at most the pipe's free space and stops at the source's end of file. A regular-file source puts page-cache references into the pipe, so later writes to the file can show through to the reader. A full pipe blocks (or `EAGAIN`, F2).
- F9: pipe to pipe: moves buffers between two pipes without copying. Both ends of the same pipe give `EINVAL`.
- F10: `len` 0: returns 0 without moving data.
- F11: invalid descriptors: `fd_in` not open for reading or `fd_out` not open for writing gives `EBADF`. Neither end a pipe, an `fd_out` opened with `O_APPEND`, or a file type that does not support splice gives `EINVAL`.
- F12: a destination pipe with no readers gives `EPIPE` and raises `SIGPIPE`. A signal during a blocking wait gives `EINTR`, or the call restarts under `SA_RESTART`.

## Determinism analysis

`linux_determinism: conditional`. The two legacy reports disagreed: https://github.com/rrnewton/hermit/issues/478 said conditional and its duplicate https://github.com/rrnewton/hermit/issues/482 said nondeterministic. Conditional wins, because every source the duplicate lists is a property of the endpoints, not of `splice` itself. If both endpoints are inside the guest and every producer and consumer runs in one deterministic schedule, the byte count, the data and the blocking point repeat. A regular-file source repeats given fixed filesystem contents. The call does not repeat when an endpoint is outside the guest (a host socket, a pipe whose other end is a host process), or when the schedule is not controlled.

### Nondeterminism sources

- How much a source pipe holds and how much space a destination pipe has, which depend on when producers and consumers run (F7, F8, F9). The pipe capacity itself is host-configurable; Detcore pins it, see Hermit design.
- Socket readiness and the amount a socket returns (F7, F8).
- Page-cache aliasing: a write to the source file after the splice, before the reader drains the pipe, changes what is read (F8).
- File offsets shared with other processes through the same open file description (F6).
- When a signal arrives during a blocking wait (F12), and host I/O errors.

## Hermit design

Target policy: `model`. The kernel keeps doing the transfer; Detcore decides when a call may block and makes sure no guest thread sleeps in the kernel where the scheduler cannot see it.

Target (owner decisions 9, 14 and 6): `model`, confirmed. Owner decision 9 (2026-10-04) names `splice` and `tee` for modelling. Today an ordinary run refuses the call with a fixed `ENOSYS` (`detcore/src/lib.rs:2138`-`2144`; the fail-closed default is documented at `detcore-model/src/config.rs:447`), and `--allow-unsupported-syscalls` forwards it to the host unchanged (`detcore/src/lib.rs:367`).

- Blocking (owner decision 14): Detcore never lets the call wait in the kernel. It runs the call as a probe with `SPLICE_F_NONBLOCK` added. If the probe returns `EAGAIN` and the guest did not ask for nonblocking behaviour (F2), the thread parks in the deterministic scheduler and probes again when the pipe it waits on changes, or when a signal arrives (`EINTR`, F12). A form whose wait Detcore cannot observe, a non-pipe end that blocks on its own (a socket, or a pipe shared with a host process), keeps today's `ENOSYS` until a wait for that end exists.
- Errno order (owner decision 6): the kernel performs every check, so Detcore's own pre-checks (the two-descriptor guest-pipe rule below) may only choose the route and must never return an errno of their own. Linux's order (`fs/splice.c`, `SYSCALL_DEFINE6(splice)`, `__do_splice`, `do_splice`; measured natively on the measurement host, Linux 7.1.3, 2026-10-04): `len` 0 returns 0 before any other check, even with bad flags or a closed descriptor (F10); flag bits outside `SPLICE_F_ALL` give `EINVAL` (F5); a descriptor that is not open gives `EBADF`; an offset pointer for a pipe end gives `ESPIPE`, `off_in` checked before `off_out`, both before either pointer is read; the pointers are then read, `off_out` before `off_in`, and an unreadable one gives `EFAULT` (F6); then `fd_in` not open for reading or `fd_out` not open for writing gives `EBADF` (F11), so a pipe's write end passed as `fd_in` with an offset pointer gives `ESPIPE`, not `EBADF`. Then by case: pipe to pipe, the same pipe gives `EINVAL` (F9); pipe to file, an offset on a file that cannot `pwrite` gives `EINVAL`, an `O_APPEND` destination `EINVAL`, then the range check; file to pipe, an offset on a file that cannot `pread` gives `EINVAL`, then the range check; neither end a pipe gives `EINVAL` (F11). T10 checks the pairs where two errors compete.

What the code does at hermit f4055eb9cae4:

- `splice` is classified `Determinized` with a comment that Detcore does not model pipe-buffer ownership (`detcore/src/syscall_classification.rs:497`) and is a member of `is_zero_copy_pipe_syscall` (`detcore/src/syscall_classification.rs:1224`), together with `tee` and `vmsplice`.
- The dispatch arm (`detcore/src/lib.rs:2138`-`2144`) returns `ENOSYS` when `panic_on_unsupported_syscalls` is set, and otherwise forwards the call through `passthrough` (`detcore/src/lib.rs:367`), which is `record_or_replay_preserving_tool_errors`. The config doc (`detcore-model/src/config.rs:447`) says an ordinary `hermit run` enables the fail-closed policy and only `--allow-unsupported-syscalls` turns it off. The family is in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), which is why `code_facts` shows `refused_only_when_fail_closed: true`.
- `hermit-cli/tests/zero_copy_pipe_fallback.rs:34` checks both states on ptrace: `ENOSYS` under `run --verify`, and a working host `splice` under the opt-out.

Why the current state is not the target:

- `ENOSYS` is not an outcome Linux produces for `splice`. A caller that falls back to `read`/`write` on `ENOSYS` keeps working; one that does not, fails. The refusal is deterministic but not faithful.
- The opt-out forwarding has two defects by reading of the code. First, Detcore makes every guest pipe physically `O_NONBLOCK` when it sequentializes threads (`detcore/src/syscalls/files.rs:3584`-`3586`), and hides that from the guest's `F_GETFL` (comment at `detcore/src/syscalls/files.rs:3582`). Because the kernel treats a pipe-side `O_NONBLOCK` as `SPLICE_F_NONBLOCK` (F2), a guest's blocking `splice` on an empty guest pipe should return `EAGAIN` instead of waiting. https://github.com/rrnewton/hermit/issues/3677 records this defect. It was found by reading the code at c9f58b2de6eb and has not been measured; T4 measures it. The cause is wider than the opt-out: the pipes are made `O_NONBLOCK` whenever threads are sequentialized, and the flag only opens the forwarding route to the bug. Second, a non-pipe end that blocks (a socket) sleeps inside the kernel, out of the scheduler's sight.
- The comment on the arm (`detcore/src/lib.rs:2133`-`2137`) says the opt-out is "used by the existing rr splice test". At this SHA that test, `rr_splice` (`hermit-cli/tests/rr_suite.rs:478`), passes no opt-out flag (`hermit-cli/tests/rr_suite.rs:197`-`202`) and is `#[ignore]`d (`hermit-cli/tests/rr_suite.rs:233`), so the comment is stale.

How to realize `model`:

- Route `splice` through the same path as reads and writes on guest pipes, `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`). For a guest pipe it runs the call nonblocking and retries under the `InternalIOPolling` resource (`detcore/src/syscalls/helpers.rs:254`-`267`), so a wait becomes a deterministic scheduling point.
- That path decides "guest pipe or not" from a single descriptor (`syscall_targets_internal_fd`, `detcore/src/syscalls/helpers.rs:797`; `get_fd`, `detcore/src/syscalls/helpers.rs:815`, covers single-descriptor syscalls only). `splice` has two descriptors and needs a two-descriptor rule: retry only if every pipe end is a guest pipe; treat a call with an external end as blocking external I/O.
- Return `EAGAIN` only when the guest asked for it: `SPLICE_F_NONBLOCK` in `flags`, or `O_NONBLOCK` in the guest's own view of the descriptor. Otherwise retry until the call can complete (F2, F7, F8).
- Short counts then follow the pinned pipe capacity, 8 KiB (`DETERMINISTIC_PIPE_CAPACITY_BYTES`, `detcore/src/syscalls/files.rs:256`, applied at `detcore/src/syscalls/files.rs:3612`), exactly as they do for `write`. Natively a new pipe holds 64 KiB, so a large file-to-pipe `splice` returns a smaller count under Hermit, but the same count on every run.
- Record/replay: in record/replay modes the guest-pipe path hands a read or write to the recorder, so its data is captured on record and reproduced on replay (`detcore/src/syscalls/helpers.rs:262`-`266`); a `splice` between guest pipes can use the same route. A transfer from a host socket or a host file into a guest pipe moves data the guest never sees in its own memory; the recorder has to capture those bytes and replay has to write them into the pipe.

Why not the other policies:

- `refuse` (today's default) breaks every caller without an `ENOSYS` fallback, for a call that every Linux kernel supports.
- `passthrough` has the two defects above.
- `emulate` with `read`/`write` through a buffer in the tracee would lose the zero-copy page semantics of F8 and the offset rules of F6 would have to be re-implemented, for no gain over letting the kernel do the transfer.

## Recommended tests

- T1 (covers F7, F10): under `hermit run --verify` on ptrace, `splice` from a pipe to `/dev/null` and to a regular file; `len` 0 returns 0; an empty pipe with no writers returns 0; an empty pipe whose writer is another guest thread waits until that thread writes, at the same point on every run.
- T2 (covers F6, F8): `splice` from a regular file into a pipe, once with `off_in` NULL (the file position advances) and once with a pointer (the offset is updated, the position does not move); a `len` above 8 KiB returns the pinned capacity; at end of file it returns 0.
- T3 (covers F9): `splice` between two guest pipes moves the data; both ends of the same pipe give `EINVAL`.
- T4 (covers F2): `SPLICE_F_NONBLOCK` on an empty pipe gives `EAGAIN`; a blocking `splice` on an empty guest pipe whose descriptor the guest never made nonblocking must wait for the writer and must not return `EAGAIN`. Run the second case also under `--allow-unsupported-syscalls` as the measurement https://github.com/rrnewton/hermit/issues/3677 lacks; that run records today's opt-out behaviour and is not a pass condition for `model`.
- T5 (covers F1, F3, F4, F5): `SPLICE_F_MOVE`, `SPLICE_F_MORE` and `SPLICE_F_GIFT` are accepted and change no result; flag `0x10` gives `EINVAL`.
- T6 (covers F11): `EBADF` for a write-only `fd_in`; `EINVAL` when neither end is a pipe and when `fd_out` has `O_APPEND`; `ESPIPE` for an offset pointer on the pipe end.
- T7 (covers F12): `EPIPE` and `SIGPIPE` for a destination pipe with no readers; a signal with a handler interrupts a waiting `splice` with `EINTR`, at the same point on every run.
- T8 (covers F8): record and replay a guest that splices from a host-backed file or a socket into a pipe and reads the pipe; replay reproduces the bytes read.
- T9 (covers none): regression check of today's behaviour. Existing: `hermit-cli/tests/zero_copy_pipe_fallback.rs:34` (`ENOSYS` under `run --verify`, host forwarding under the opt-out) and the e2e cell `c-programs/splice-enosys` (`tests/e2e/manifests/c-programs.yaml:9080`, verify with `ptrace: true` at line 9099), both on `tests/c/splice_enosys.c`. They test the refusal, not the target, so they cover no flag; their `ENOSYS` expectation has to change when `model` lands.
- T10 (covers F5, F6, F10, F11): errno order (owner decision 6). Run each case natively first and require Hermit's result to equal it: `len` 0 with flag `0x10`, and `len` 0 with a closed `fd_in`, return 0; flag `0x10` with a closed descriptor and `len` 1 gives `EINVAL`; a pipe's write end as `fd_in` gives `ESPIPE` with an `off_in` pointer and `EBADF` without one; an `off_in` pointer on a pipe `fd_in` together with an invalid `off_out` pointer gives `ESPIPE`, not `EFAULT`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/478: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_zero_copy_pipe_syscall, refused only when fail-closed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/482 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel moves the bytes, Detcore owns blocking via the guest-pipe nonblocking retry path; today ordinary runs refuse with ENOSYS and the opt-out forwards to the host); linux_determinism stays conditional over the duplicate's nondeterministic; merged https://github.com/rrnewton/hermit/issues/482; 12 flag behaviours, 9 tests; existing tests check only the ENOSYS refusal, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 14 and 6 applied (decision 9 names splice for modelling; new `Target (owner decisions 9, 14 and 6)` paragraph with the probe-and-park wait and Linux's check order, confirmed natively on Linux 7.1.3); target_policy model -> model unchanged; new T10 (errno order); the opt-out EAGAIN hypothesis now cites https://github.com/rrnewton/hermit/issues/3677 as code-read and unmeasured, T4 wording updated; removed the open question (keep ENOSYS or model), answered by decision 9

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `splice`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+splice&body=Discussion+for+the+%60splice%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsplice.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60splice%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: splice" (https://github.com/rrnewton/hermit/issues/478), legacy variant A1. Its duplicate https://github.com/rrnewton/hermit/issues/482 was filed the same day with the same title (legacy variant A1, batch 5 of the classification sweep) and is merged into the sections above. The comment histories stay on the two issues; https://github.com/rrnewton/hermit/issues/482 had 0 comments at retirement.

Merge notes:

- Determinism: https://github.com/rrnewton/hermit/issues/478 said conditional, https://github.com/rrnewton/hermit/issues/482 said NONDETERMINISTIC. Conditional was kept; the reason is under Determinism analysis.
- Recommended action: both said NEEDS-HANDLER. This body chooses `model`.
- Both reports said Hermit routed `splice` through the UNCLASSIFIED temporary-passthrough block of `detcore/src/syscall_classification.rs` (at `dd60278f` for the duplicate). That was true when filed and is stale at f4055eb9cae4, where the call is refused by default (Hermit design).
- The duplicate's source list (producer timing, pipe capacity, file offsets, socket readiness, signals, host I/O errors) is kept under Nondeterminism sources.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/478)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=splice` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. The call was 4 bytes from `/etc/hostname` into a pipe; it returned 3.

Observed strace line:
```
splice(3, NULL, 6, NULL, 4, 0)  = 3
```

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/482)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=splice`. The harness exited 0. The probe moved one byte between two private pipes.

Observed strace line:
```text
splice(3, NULL, 6, NULL, 1, 0)  = 1
```

Both reports were produced by the autonomous syscall-classification sweep (ranks 151-250, and batch 5 covering ranks 201-250, of the then-UNCLASSIFIED list), from host testing and manual research, with no code change proposed.
