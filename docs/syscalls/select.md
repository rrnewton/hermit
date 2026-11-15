---
schema: syscall-pages/v1
syscall: select
nr_x86_64: 23
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/424
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+select&body=Discussion+for+the+%60select%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fselect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60select%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 9
---

## Summary

`select(nfds, readfds, writefds, exceptfds, timeout)` waits until a descriptor in one of three bitmaps is ready, or until a `timeval` timeout expires. It rewrites the bitmaps and the remaining time in place. Detcore models it like `pselect6`. Under sequentialized threads it polls with zero-timeout probes on the deterministic schedule and measures the timeout in virtual time. Record/replay captures the result, the sets and the remaining time (`detcore/src/syscalls/io.rs:729-924` at hermit f4055eb9cae4). One divergence is known: Hermit rejects a `tv_usec` of one million or more, which Linux accepts.

## Linux semantics

`select` checks descriptors `0..nfds-1` in the read, write and exception sets.

- **Return value:** the total number of ready bits, with each set rewritten to show only the ready descriptors. 0 means the timeout expired, and on error the sets are unspecified.
- **Timeout:** a `NULL` timeout waits forever, and a zero timeout polls once. Linux writes the time not slept back into `*timeout`, also on `EINTR`; a fault during that writeback does not change the result.
- **nfds:** a negative `nfds` gives `EINVAL`. An `nfds` beyond the process's fd table is clamped to the table size.
- **Errors:**
  - a closed descriptor in any set gives `EBADF`;
  - an unreadable set gives `EFAULT`, and so does an unwritable set when the results are copied back; sets copied before the faulting one have already been written;
  - a negative `tv_sec` or `tv_usec` gives `EINVAL`;
  - a signal gives `EINTR`, since `select` is not restarted after a handler runs.
- **Large `tv_usec`:** Linux normalizes a `tv_usec` of one million or more into seconds rather than rejecting it. Measured on this host (Linux 7.1.3) by `a scratch directory`:
  - `{0, 1500000}` waited 1.501 s and returned 0;
  - `{0, -1}`, `{1, -1000000}` and `{-1, 0}` each gave `EINVAL`.
- **glibc:** on x86-64, glibc's `select()` wrapper issues `pselect6` (`tests/c/record_replay_select.c:22`, `tests/rust/select.rs:16-18`), so this syscall is reached mainly by raw `syscall(SYS_select, ...)` callers and by other runtimes.

### Flags and options

- F1: read set: descriptors with data, EOF or a pending connection are reported readable.
- F2: write set: descriptors with buffer space are reported writable.
- F3: exception set: descriptors with exceptional conditions (for example TCP urgent data) are reported.
- F4: return value and in-place rewrite: the count of ready bits, with each set reduced to its ready descriptors.
- F5: a negative `nfds` gives `EINVAL`.
- F6: an `nfds` larger than the fd table is clamped (for example `FD_SETSIZE` with few open fds).
- F7: a `NULL` timeout waits until a descriptor is ready or a signal arrives.
- F8: a zero timeout polls once and returns at once.
- F9: a positive timeout waits up to that long, returns 0 on expiry, and writes the time not slept back into `*timeout`.
- F10: a `tv_usec` of one million or more is accepted and normalized by Linux; Hermit returns `EINVAL` (divergence).
- F11: a negative `tv_sec` or `tv_usec` gives `EINVAL`.
- F12: a closed descriptor in any set gives `EBADF`.
- F13: `EFAULT` for an unreadable set or timeout, and partial copy-out when a later output set is unwritable; a fault while writing the remaining time is ignored.
- F14: a signal while waiting gives `EINTR`, with the remaining time written back.

## Determinism analysis

Readiness depends on when other threads, processes or external peers act on the descriptors (F1, F2, F3), so the result count and the sets vary between runs (F4). A positive timeout races readiness against the host clock, and the remaining time written back comes from the host clock (F9). Whether a signal interrupts the wait depends on delivery timing (F14). A `NULL` timeout blocks for a host-decided time (F7). Only argument validation (F5, F10, F11, F12, F13) and a zero-timeout poll of already-settled descriptors repeat on their own. Hence `nondeterministic`.

## Hermit design

The policy is `model`, as implemented at hermit f4055eb9cae4:

- **Dispatch:** `Syscall::Select` goes to `handle_select` (`detcore/src/lib.rs:2615`, with `TODO-HUMAN-REVIEW(#800)` at `detcore/src/lib.rs:2614`).
- **Record/replay modes:** the call goes to `record_or_replay_select_family` (`detcore/src/syscalls/io.rs:742-752`). A zero timeout takes an ordinary scheduler turn and is recorded. Any other call runs as a recorded blocking external operation. The recorder captures the result, the sets and the remaining time, so replay never asks the live kernel about descriptors it did not recreate (`detcore/src/syscalls/io.rs:434-451`). The ready-pipe regression for this is https://github.com/rrnewton/hermit/issues/3569, cited in `tests/c/record_replay_select.c:11`.
- **Not sequentialized:** a raw inject (`detcore/src/syscalls/io.rs:753-755`).
- **Sequentialized run mode:**
  - a negative `nfds` is injected so the kernel returns `EINVAL` (`detcore/src/syscalls/io.rs:757-759`) (F5);
  - the timeval is read, and a fault gives `EFAULT` (`detcore/src/syscalls/io.rs:761-767`) (F13);
  - a zero timeout is injected directly (`detcore/src/syscalls/io.rs:768-771`) (F8);
  - an `nfds` above 64 (`PSELECT6_INTERNAL_MAX_NFDS`, `detcore/src/syscalls/io.rs:96`) leaves the sets to the kernel through `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:775-779`), so readiness timing for those calls is host-decided (F6);
  - otherwise the timeout is converted (`detcore/src/syscalls/io.rs:781`, `select_timeout_duration` at `detcore/src/syscalls/io.rs:198-206`) (F10, F11), and the call goes to `handle_internal_select` (`detcore/src/syscalls/io.rs:785-889`).
- **`handle_internal_select`:**
  - sets the deadline to virtual time plus the timeout (`detcore/src/syscalls/io.rs:792-795`);
  - copies the guest sets and probes with scratch sets and a scratch zero timeout on the guest stack (`detcore/src/syscalls/io.rs:796-831`);
  - loops on an InternalIOPolling resource whose signal interruption yields `EINTR` (`detcore/src/syscalls/io.rs:833-848`) (F14);
  - returns any nonzero or error probe result after copying the sets back (`detcore/src/syscalls/io.rs:860-870`) (F1-F4, F12);
  - returns 0 when the virtual deadline passes (`detcore/src/syscalls/io.rs:873-881`) (F9).
  
  `write_select_remaining` derives the remaining time from virtual time and ignores writeback faults, as Linux does (`detcore/src/syscalls/io.rs:903-924`) (F9, F13). A `NULL` timeout never reaches a deadline and retries until a descriptor is ready or a signal arrives (F7).

Why not `passthrough`: readiness and timeout are the nondeterminism, and both are already virtualized. Why not `emulate`: readiness of real kernel objects must be probed.

Target (owner decisions 6, 13, 14): the timeout check follows the kernel's order and arithmetic. `kern_select` copies the timeval (`EFAULT`), then calls `poll_select_set_timeout` with `tv_sec + tv_usec / 1000000` seconds and `(tv_usec % 1000000) * 1000` nanoseconds, which gives `EINVAL` only when the result is not a valid timespec (a negative second or nanosecond field); `core_sys_select` then checks `nfds < 0` (`EINVAL`), clamps `nfds` to the table's `max_fds` and copies the sets (`EFAULT`), and `do_select` reports `EBADF`. So `select_timeout_duration` (`detcore/src/syscalls/io.rs:198-206`) must normalize a `tv_usec` of one million or more instead of returning `EINVAL` (F10, T6). The wait stays a non-blocking probe plus a scheduler wait against a virtual deadline, as `handle_internal_select` already does (`detcore/src/syscalls/io.rs:792-795`); the `nfds > 64` path (`detcore/src/syscalls/io.rs:775-779`) must become the same, with the bitmap read clamped as in pselect6, instead of `record_or_replay_blocking`.

**Divergences and gaps that keep this `todo`:**

1. `select_timeout_duration` returns `EINVAL` for a `tv_usec` of one million or more, citing "Linux rejects select timeouts whose microsecond field is out of range" (`detcore/src/syscalls/io.rs:201-204`). The host probe above contradicts that comment (F10).
2. The run-mode probe result is returned raw (`detcore/src/syscalls/io.rs:860`), while `pselect6` maps an injected `ERESTARTSYS` to `EINTR` (`detcore/src/syscalls/io.rs:111-119`). Whether a zero-timeout `select` probe can leak a restart code is unverified.
3. In run mode, a zero-timeout `select` takes no scheduler turn (`detcore/src/syscalls/io.rs:768-771`), whereas zero-timeout `poll` yields one so that a polling thread cannot monopolize the guest between preemptions (`detcore/src/syscalls/io.rs:408-417`).

## Recommended tests

- T1 (covers F1, F4, F6, F8, F9): raw `SYS_select` on a ready pipe with a 5 s timeout, with a zero timeout, with `nfds == FD_SETSIZE`, and on an empty pipe with 10 ms. Replay must reproduce the result, the sets and the remaining time. Existing candidate: `record_raw_select_ready_pipe` in `hermit-cli/tests/record_replay.rs:2253` (guest `tests/c/record_replay_select.c`, mode `raw`).
- T2 (covers F13): raw select whose write set is on a read-only page writes the read set, then gives `EFAULT`, in record and replay. Existing candidate: `record_select_partial_fd_set_copyout` in `hermit-cli/tests/record_replay.rs:2268`.
- T3 (covers F5): raw select with a negative `nfds` gives `EINVAL` in record and replay. Existing candidate: `record_select_negative_nfds_preserves_einval` in `hermit-cli/tests/record_replay.rs:2273`.
- T4 (covers F7): a second thread blocks in raw select with a `NULL` timeout on an empty pipe that the main thread then fills, and replay does not wait on the live pipe. Existing candidate: `record_select_null_timeout_woken_by_thread` in `hermit-cli/tests/record_replay.rs:2283`.
- T5 (covers F1, F9): the ready path and the 10 ms timeout path of raw select under `hermit run --verify` on ptrace (run mode, `handle_internal_select`). Existing guest: `tests/rust/select.rs` (cargo target `rustbin_select`, used in record/replay by `record_rs_select` at `hermit-cli/tests/record_replay.rs:2334`). It has no e2e manifest cell at f4055eb9cae4, so run-mode coverage needs one.
- T6 (covers F10, F11): a timeout of `{0, 1500000}` must wait 1.5 virtual seconds and return 0, as Linux does. `{0, -1}` and `{-1, 0}` must give `EINVAL`. This fails today on the first case.
- T7 (covers F2, F3, F12): a write set on a pipe with free space reports writable; an exception set on a loopback TCP socket after `send(..., MSG_OOB)` reports the exception, and on a pipe reports nothing; a closed fd in the read set gives `EBADF`, identically across runs.
- T8 (covers F14): a timer signal with a handler interrupts a raw select that has a 10 s timeout. Expect `EINTR`, and a remaining time derived from virtual time that is identical across runs under `--verify`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/424: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · facts-bot · code_facts changed: tests_naming 1 -> 2.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (virtual-time zero-timeout probe loop, recorded sets and remaining time in record/replay); linux_determinism stays nondeterministic; 14 flags, T1-T8 proposed; open questions: tv_usec >= 1e6 is EINVAL under Hermit but normalized by Linux, raw probe result vs pselect6 ERESTARTSYS mapping, no scheduler turn on zero timeout; tracking_status stays todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 6 (normalize tv_usec >= 1000000 as kern_select does, kernel check order stated) and decisions 13 and 14 (nfds > 64 becomes probe plus virtual-time wait); target_policy unchanged; no new F/T ids; removed the tv_usec normalization question.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by hermit-cli/tests/record_replay.rs, T2 covered by hermit-cli/tests/record_replay.rs, T3 covered by hermit-cli/tests/record_replay.rs, T4 covered by hermit-cli/tests/record_replay.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `select`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+select&body=Discussion+for+the+%60select%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fselect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60select%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: select" (https://github.com/rrnewton/hermit/issues/424), legacy variant A1.

The 2026-07-24 evidence below describes how the call was routed at that time. At hermit f4055eb9cae4, `select` has its own typed handler (see Hermit design), and the "temporary-passthrough" wording no longer applies.

### Open questions

- Should the run-mode probe at `detcore/src/syscalls/io.rs:860` map `ERESTARTSYS` to `EINTR` as `pselect6_probe_result` does (`detcore/src/syscalls/io.rs:111-119`)?
- Should a zero-timeout `select` in sequentialized run mode take a scheduler turn as zero-timeout `poll` does (`detcore/src/syscalls/io.rs:408-417` versus `detcore/src/syscalls/io.rs:768-771`)?

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=select` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `select` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
select(0, NULL, NULL, NULL, {tv_sec=0, tv_usec=0}) = 0 (Timeout)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
