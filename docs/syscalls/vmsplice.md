---
schema: syscall-pages/v1
syscall: vmsplice
nr_x86_64: 278
linux_determinism: nondeterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/640
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+vmsplice&body=Discussion+for+the+%60vmsplice%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fvmsplice.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60vmsplice%60%2C+or+in+what+the+page+says%3F%0A
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

`vmsplice(fd, iov, nr_segs, flags)` moves data between user memory and a pipe: on a write end it puts references to the caller's pages into the pipe, and on a read end it copies pipe data into the caller's buffers. At hermit f4055eb9cae4, an ordinary `hermit run` refuses it with `ENOSYS` (`detcore/src/lib.rs:2138`), and only the `--allow-unsupported-syscalls` opt-out forwards it to the host. The target is to emulate it with copy semantics, as a `writev` or `readv` on the pipe under Detcore's control of blocking.

Manual reference: [vmsplice(2)](https://man7.org/linux/man-pages/man2/vmsplice.2.html).

## Linux semantics

The direction comes from the descriptor's open mode. On a pipe's write end, the kernel takes references to the user pages named by `iov` and queues them in the pipe without copying. The data a reader later gets is whatever those pages hold when the reader consumes them, not necessarily what they held when `vmsplice` returned. The man page therefore tells callers not to modify the memory until it has been consumed. On a read end (supported since Linux 2.6.23), `vmsplice` copies from the pipe into `iov`, like `readv`.

The return value is the number of bytes transferred. It can be shorter than requested: a write end takes at most the pipe's free space, and a read end returns at most what the pipe holds. A full pipe (write end) or an empty pipe that still has writers (read end) blocks, unless `SPLICE_F_NONBLOCK` is given. Unlike `splice` and `tee`, `vmsplice` never looks at the descriptor's `O_NONBLOCK`: in `fs/splice.c` the `vmsplice` entry point never translates it, and `wait_for_space` and `splice_from_pipe_next` test only `SPLICE_F_NONBLOCK`. Measured natively on the measurement host (Linux 7.1.3, 2026-10-04): with flags 0, a write-end `vmsplice` on a full pipe opened with `O_NONBLOCK`, and a read-end one on an empty such pipe with a writer, each waited until a signal interrupted it (`EINTR`); with `SPLICE_F_NONBLOCK` the write end gave `EAGAIN`.

### Flags and options

- F1: write end (descriptor open for writing): queues references to the caller's pages in the pipe, without copying; returns the bytes queued, at most the pipe's free space.
- F2: read end (descriptor open for reading): copies pipe data into the `iov` buffers, like `readv`; an empty pipe with no writers returns 0.
- F3: `SPLICE_F_GIFT` (0x8), write end only: the pages are a gift, which lets a later `splice` with `SPLICE_F_MOVE` take them over; the caller promises never to modify them again. Ignored on a read end.
- F4: `SPLICE_F_NONBLOCK` (0x2): return `EAGAIN` instead of blocking on a full pipe (write end) or an empty pipe with writers (read end). `O_NONBLOCK` on the descriptor has no effect: without `SPLICE_F_NONBLOCK` the call waits even on a nonblocking pipe.
- F5: `SPLICE_F_MOVE` (0x1) and `SPLICE_F_MORE` (0x4): accepted, with no effect on `vmsplice`.
- F6: any flag bit outside `SPLICE_F_ALL` (0xf): `EINVAL`.
- F7: the `iov` array: `nr_segs` 0 (or a total length of 0) returns 0; `nr_segs` above `IOV_MAX` (1024) gives `EINVAL`; an unreadable `iov` array, or user memory the kernel cannot access, gives `EFAULT`.
- F8: invalid descriptor: not open, not a pipe, or open for neither reading nor writing (`O_PATH`) gives `EBADF`. The not-a-pipe case is checked only after the `iov` (F7), so on a descriptor that is not a pipe a total length of 0 returns 0 and an unreadable `iov` array gives `EFAULT`.
- F9: page aliasing after a write-end call: stores the caller makes to the pages after `vmsplice` returns, but before a reader consumes them, are visible to the reader.
- F10: a write end whose pipe has no readers gives `EPIPE` and raises `SIGPIPE`. A signal during a blocking wait gives `EINTR`, or the call restarts under `SA_RESTART`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Like `write` and `read` on a pipe, the count and the blocking point depend on how full the pipe is, and so on producer and consumer timing (F1, F2, F4). That part repeats under one deterministic schedule of every process using the pipe. What sets `vmsplice` apart is F9: the effect of a write-end call extends past its return. The bytes the reader sees depend on how the writer's later stores interleave with the reader's consumption, which is a cross-process memory race that no syscall boundary orders. Under Linux that race is present in every write-end call whose caller reuses its buffer before the data is consumed, which is the case the man page warns about, and the call gives no way to learn when consumption happened.

### Nondeterminism sources

- Pipe fill level and free space, set by producer and consumer timing (F1, F2, F4). The pipe capacity itself is host-configurable; Detcore pins it, see Hermit design.
- Page aliasing: the writer's stores after the call versus the reader's consumption (F9). This also means a record of the bytes taken at `vmsplice` time can differ from the bytes the reader got.
- When a signal arrives during a blocking wait (F10).
- A pipe end held by a process outside the guest.

## Hermit design

Target policy: `emulate`. Detcore should perform a write-end `vmsplice` as a `writev` of the same `iov` on the same pipe, and a read-end `vmsplice` as a `readv`, both through the deterministic pipe path, returning the count and errno Linux would return.

Target (owner decisions 9, 6 and 14): `emulate`, as a copy. Owner decision 9 (2026-10-04) says to handle `vmsplice` as a copy, which is this body's target: the bytes are copied at the call, so no page reference outlives it. Today an ordinary run refuses the call with a fixed `ENOSYS` (`detcore/src/lib.rs:2138`-`2144`; the fail-closed default is documented at `detcore-model/src/config.rs:447`), and `--allow-unsupported-syscalls` forwards it to the host unchanged (`detcore/src/lib.rs:367`).

- Errno order (owner decision 6): Detcore performs the checks itself, so it must follow Linux's order (`fs/splice.c`, `SYSCALL_DEFINE4(vmsplice)`, `vmsplice_type`, `vmsplice_to_pipe`, `vmsplice_to_user`; measured natively on the measurement host, Linux 7.1.3, 2026-10-04): flag bits outside `SPLICE_F_ALL` give `EINVAL` before the descriptor is looked at (F6); a descriptor that is not open, or open for neither writing nor reading (`O_PATH`), gives `EBADF` (F8), and the direction is "write" if the descriptor is writable and "read" otherwise, so a read-write FIFO descriptor is a write end; then the `iov` is imported: `nr_segs` above 1024 gives `EINVAL`, an unreadable array `EFAULT` (F7); a total length of 0 returns 0 on any descriptor (F7); only then a descriptor that is not a pipe gives `EBADF` (F8). On a write end, a pipe with no readers then gives `EPIPE` and `SIGPIPE` (F10), and a full pipe waits, or gives `EAGAIN` with `SPLICE_F_NONBLOCK` (F4); a read end copies as `readv` would, under the same wait rule. T9 checks the pairs where two errors compete.
- Blocking (owner decision 14): the copy runs through the guest-pipe path (`execute_nonblockable_fd_syscall`, `detcore/src/syscalls/helpers.rs:202`) as a nonblocking `writev` or `readv` retried under `InternalIOPolling` (`detcore/src/syscalls/helpers.rs:254`-`267`), so a wait is a deterministic scheduler wait. The decision to return `EAGAIN` instead of waiting comes only from `SPLICE_F_NONBLOCK`, never from the guest's own view of `O_NONBLOCK` (F4); this is where the emulation differs from a guest `writev`. A `vmsplice` on a pipe that is not a guest pipe (shared with a host process) keeps today's `ENOSYS` until a wait for it exists.

What the code does at hermit f4055eb9cae4:

- `vmsplice` is classified `Determinized` with a comment that Detcore does not model "vmsplice page pinning" (`detcore/src/syscall_classification.rs:497`-`507`) and belongs to `is_zero_copy_pipe_syscall` (`detcore/src/syscall_classification.rs:1224`), whose doc says `vmsplice` "can additionally pin guest pages beyond the syscall boundary" (`detcore/src/syscall_classification.rs:1220`-`1221`).
- The dispatch arm (`detcore/src/lib.rs:2138`-`2144`) returns `ENOSYS` when `panic_on_unsupported_syscalls` is set, which an ordinary `hermit run` does (`detcore-model/src/config.rs:447`), and otherwise forwards through `passthrough` (`detcore/src/lib.rs:367`). The family is in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), hence `refused_only_when_fail_closed: true` in `code_facts`.
- `hermit-cli/tests/zero_copy_pipe_fallback.rs:34` checks both states on ptrace with `tests/c/vmsplice_enosys.c`: `ENOSYS` under `run --verify`, and under the opt-out a one-byte write-end `vmsplice` whose byte is read back.

Why `emulate` with copy semantics:

- Copying removes F9. For a caller that keeps the man page's rule and does not touch the memory before it is consumed, a copy taken at the call is indistinguishable from the shared pages: the reader gets the same bytes. A caller that breaks the rule is racing, and a copy turns that race into one fixed answer (the bytes at call time), which is one of the outcomes Linux allows.
- The copy makes the data a function of the guest's memory at a known scheduling point, so record and replay see the same bytes the reader consumes. With forwarded page references they need not.
- Detcore already has the parts: guest pipes are made physically `O_NONBLOCK` and pinned to 8 KiB (`detcore/src/syscalls/files.rs:3584`-`3586`, `detcore/src/syscalls/files.rs:256`, `detcore/src/syscalls/files.rs:3612`), and `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`) runs a pipe operation nonblocking and retries it under `InternalIOPolling` (`detcore/src/syscalls/helpers.rs:254`-`267`). In record/replay modes the same path routes the data through the recorder (`detcore/src/syscalls/helpers.rs:262`-`266`). The emulation must return `EAGAIN` only when the guest passed `SPLICE_F_NONBLOCK` (F4); unlike a guest `write`, it must wait even when the guest's own view of the descriptor has `O_NONBLOCK`.
- `SPLICE_F_GIFT` (F3) is only a hint that a later `splice` may take over the pages; with a copy there is nothing to take over, and dropping the hint changes no result. Argument checks (F6, F7, F8) must be done first, in the kernel's order (listed under `Target` above), so a bad call gets the same errno as on Linux.

Why not the other policies:

- `refuse` (today's default) returns `ENOSYS`, which Linux never does for `vmsplice`; callers without a `write` fallback fail.
- `passthrough` keeps F9, so the reader's bytes depend on an unordered race. Under the opt-out the physical `O_NONBLOCK` on guest pipes (`detcore/src/syscalls/files.rs:3584`-`3586`) does not turn a blocking call into `EAGAIN`, because `vmsplice` ignores `O_NONBLOCK` (F4). Instead a blocking call on a full or empty guest pipe sleeps in the kernel while its thread holds the scheduler's turn; by reading of the code, not measured, that can hang a run whose other side is another guest thread. https://github.com/rrnewton/hermit/issues/3677 groups `vmsplice` with `splice` and `tee` as returning a spurious `EAGAIN`; for `vmsplice` that is wrong.
- `model` (forward the call nonblocking, as for `splice`) also keeps F9.

`linux_determinism` stays `nondeterministic`: emulation removes the aliasing source under Hermit, but the classification describes Linux.

## Recommended tests

- T1 (covers F1, F9): under `hermit run --verify` on ptrace, a write-end `vmsplice` of a multi-segment `iov`; read the pipe in the same and in another guest process and check the bytes. Then overwrite the buffer after the call and before the reader runs, and check that the reader gets the bytes from the time of the call on every run.
- T2 (covers F2): a read-end `vmsplice` into a multi-segment `iov` returns the pipe's data in order; an empty pipe with no writers returns 0.
- T3 (covers F3, F5, F6): `SPLICE_F_GIFT`, `SPLICE_F_MOVE` and `SPLICE_F_MORE` are accepted and change no result; flag `0x10` gives `EINVAL`.
- T4 (covers F4): `SPLICE_F_NONBLOCK` on a full pipe (write end) and on an empty pipe with a writer (read end) gives `EAGAIN`; without it, on a guest pipe the guest never made nonblocking, the call waits for the other side at the same point on every run. A pipe the guest itself created with `O_NONBLOCK` (`pipe2`), with flags 0, also waits and does not return `EAGAIN`, as it does natively.
- T5 (covers F7, F8): `nr_segs` 0 returns 0; `nr_segs` 1025 gives `EINVAL`; a bad `iov` pointer gives `EFAULT`; a regular-file descriptor, a closed descriptor and an `O_PATH` descriptor give `EBADF`.
- T6 (covers F1, F10): a write-end call on a pipe whose reader closed gives `EPIPE` and `SIGPIPE`; a call larger than the pinned 8 KiB capacity returns a short count or waits, as Linux would for that capacity; a signal with a handler interrupts a waiting call with `EINTR`.
- T7 (covers F1, F2): record and replay a guest that moves data through a pipe with write-end and read-end `vmsplice`; replay reproduces every count and every byte read.
- T8 (covers none): regression check of today's behaviour. Existing: `hermit-cli/tests/zero_copy_pipe_fallback.rs:34` and the e2e cell `c-programs/vmsplice-enosys` (`tests/e2e/manifests/c-programs.yaml:10734`, verify with `ptrace: true` at line 10753), both on `tests/c/vmsplice_enosys.c`. They test the refusal, not the target, so they cover no flag; their `ENOSYS` expectation has to change when `emulate` lands.
- T9 (covers F6, F7, F8): errno order (owner decision 6). Run each case natively first and require Hermit's result to equal it: flag `0x10` with a closed descriptor and `nr_segs` 1025 gives `EINVAL`; an `O_PATH` descriptor with `nr_segs` 1025 gives `EBADF`; a pipe with `nr_segs` 1025 gives `EINVAL`; a read-only regular file gives `EBADF` with a one-byte `iov`, 0 with `nr_segs` 0, and `EFAULT` with an invalid `iov` pointer.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/640: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_zero_copy_pipe_syscall, refused only when fail-closed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (perform a write-end vmsplice as writev and a read-end one as readv on the deterministic pipe path, which removes the page-aliasing race; today ordinary runs refuse with ENOSYS); linux_determinism stays nondeterministic because of that aliasing; 10 flag behaviours, 8 tests; existing tests check only the ENOSYS refusal, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6 and 14 applied (decision 9's 'vmsplice as a copy' is this body's emulate target; new `Target (owner decisions 9, 6 and 14)` paragraph with Linux's check order and the scheduler wait, confirmed natively on Linux 7.1.3); target_policy emulate -> emulate unchanged; F4 narrowed (O_NONBLOCK has no effect on vmsplice, only SPLICE_F_NONBLOCK counts), F8 narrowed (the not-a-pipe EBADF comes after the iov checks); Linux semantics and the passthrough bullet corrected, noting https://github.com/rrnewton/hermit/issues/3677 is wrong about vmsplice; T4 hypothesis wording replaced; new T9 (errno order); removed the open question (copy emulation or refuse), answered by decision 9
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T8 covered by tests/c/vmsplice_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `vmsplice`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+vmsplice&body=Discussion+for+the+%60vmsplice%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fvmsplice.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60vmsplice%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: vmsplice" (https://github.com/rrnewton/hermit/issues/640), legacy variant B. Its comment history stays on that issue.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

The legacy summary asked to replace a temporary catch-all policy with an explicit Detcore classification; that is done at f4055eb9cae4 (`detcore/src/syscall_classification.rs:507`), and the legacy determinism note (pipe readiness, partial transfer and gifted-page lifetime need deterministic I/O handling) is kept under Determinism analysis.
