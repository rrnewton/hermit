---
schema: syscall-pages/v1
syscall: pselect6
nr_x86_64: 270
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/384
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pselect6&body=Discussion+for+the+%60pselect6%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpselect6.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pselect6%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 13
---

## Summary

`pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask_arg)` waits until a descriptor in one of three bitmaps is ready, the timeout expires, or a signal arrives. While it waits, it can swap in a temporary signal mask atomically. Detcore models the wait (`detcore/src/syscalls/io.rs:456`). Under sequentialized `hermit run`, it replaces the kernel's sleep with zero-timeout probes against a deadline in virtual time, and each probe carries the snapshotted temporary mask. Record/replay records the call's outputs.

Manual reference: Linux man-pages select(2) (section "pselect6"), signal(7).

## Linux semantics

`pselect6` is the kernel entry point behind glibc's `pselect()`. The header of `tests/c/record_replay_select.c` says x86-64 glibc also issues `select()` as `pselect6`. The kernel call differs from the glibc wrapper in two ways:

- the sixth argument is a pointer to a two-word wrapper `{ const sigset_t *ss; size_t ss_len; }`, not a `sigset_t *`
- the timeout is in-out: the kernel writes the time it did not sleep back into `*timeout`. glibc passes a private copy, so callers of the wrapper never see this.

The kernel checks arguments in this order:

1. copy the sigmask wrapper (`EFAULT`)
2. copy the timeout (`EFAULT`)
3. validate the timeout (`EINVAL`)
4. install the inner mask (`EINVAL` for a bad size, `EFAULT` for an unreadable mask)
5. check `nfds` and copy the descriptor sets

On return, it writes the three sets back so they hold only the ready descriptors, writes back the remaining time, and restores the original signal mask. If the remaining-time copy faults, the result is kept. `nfds` is clamped to the size of the process's descriptor table. That table starts at one machine word (64 descriptors) and grows as descriptors are opened.

Readiness is decided by whatever else touches the descriptors: other threads, other processes, the network, terminals. A signal handler that runs during the wait ends it with `EINTR`. Per signal(7), `pselect` is never restarted after a handler, even with `SA_RESTART`.

### Flags and options

- F1: `nfds`: one more than the highest descriptor to test. Negative gives `EINVAL`. A value larger than the descriptor table is clamped to the table size; it is not an error.
- F2: `readfds`, `writefds`, `exceptfds`: in-out bitmaps, any of which may be NULL. On return each holds only the ready descriptors, and the call returns the total count of set bits (0 on timeout). A set bit naming a descriptor that is not open gives `EBADF`.
- F3: NULL `timeout`: wait without a time limit.
- F4: zero `timeout` (`{0, 0}`): test readiness once and return immediately. A zero timeout is not written back.
- F5: finite `timeout`: wait up to that long. The raw syscall writes the unslept remainder back to `*timeout`, also after `EINTR` and after an `EFAULT` on the sets. A fault during that copy-out is ignored and the result kept.
- F6: malformed `timeout` (negative `tv_sec`, or `tv_nsec` outside `[0, 1e9)`) gives `EINVAL`. An unreadable `timeout` gives `EFAULT`.
- F7: sigmask wrapper NULL, or wrapper with a NULL inner pointer: the signal mask is not changed.
- F8: sigmask wrapper with a non-NULL inner pointer: `ss_len` must be 8 (`EINVAL` otherwise). The mask replaces the thread's mask atomically for the duration of the wait and is restored on return. An unreadable wrapper or inner mask gives `EFAULT`.
- F9: argument validation order, as listed above: wrapper `EFAULT`, then timeout `EFAULT`, then timeout `EINVAL`, then mask size `EINVAL` and mask `EFAULT`, then `nfds` and the sets.
- F10: unreadable or unwritable descriptor sets give `EFAULT`, and the remaining time is still written back.
- F11: a signal handler runs during the wait: `EINTR`, never restarted even with `SA_RESTART`. If no handler runs (for example a stop and continue), the kernel restarts the call transparently.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. When a descriptor becomes ready, whether the timeout fires first, and how much time is written back all depend on host scheduling and on a real clock. Whether a signal lands inside the wait depends on signal arrival time. The call repeats only for a zero timeout against descriptors whose state is already settled, for example `nfds = 0` or a pipe that is already readable.

### Nondeterminism sources

- Readiness timing of the descriptors, set by peer threads, processes and outside I/O
- Timeout expiry against the real clock, and the remaining time written back
- Signal arrival relative to the wait, which decides between `EINTR` and a normal return
- The temporary mask, which decides which of those signals can interrupt the wait

## Hermit design

Target policy: `model`. Detcore keeps the kernel as the judge of readiness and of argument errors, and replaces the sleep with a wait it schedules itself in virtual time. `handle_pselect6` (`detcore/src/syscalls/io.rs:456`) chooses among four paths:

- **Record/replay** (`detcore/src/syscalls/io.rs:461-471`). The call goes through `record_or_replay_select_family` (`detcore/src/syscalls/io.rs:439-451`). A zero timeout takes a scheduler turn and is recorded. Any other timeout runs as `record_or_replay_blocking`, which deschedules the thread as blocking external I/O. The recorder captures the sets and the remaining time, so replay does not ask the live kernel.
- **Unsequentialized run** (`detcore/src/syscalls/io.rs:472-474`): injected unchanged.
- **Sequentialized run, kernel-owned cases.**
  - A negative `nfds` is injected so the kernel returns `EINVAL` (`detcore/src/syscalls/io.rs:476-478`).
  - A zero timeout is injected as is (`detcore/src/syscalls/io.rs:526-528`).
  - `nfds` above 64 goes to `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:534-538`), because the kernel's clamp to the table size cannot be reproduced from userspace.
- **Sequentialized run, modelled wait** (`handle_internal_pselect6`, `detcore/src/syscalls/io.rs:572-691`):
  - Before reading the wrapper, Detcore injects a validation probe: `nfds = 0` with the deliberately malformed timeout `{0, 1e9}` (`detcore/src/syscalls/io.rs:480-517`). This makes the kernel check the wrapper first, so F9 is kept, and the only accepted answer is `EINVAL`.
  - The timeout is read and validated (`ppoll_timeout_duration`, `detcore/src/syscalls/io.rs:189-196`).
  - A non-NULL inner mask must have size 8 (`detcore/src/syscalls/io.rs:549-553`). The mask is read and snapshotted, with Hermit's perf-event signal bit cleared by `sanitize_ppoll_signal_mask` (`detcore/src/syscalls/io.rs:386-389`). The wrapper is then dropped from the call, so later changes the guest makes to it cannot alter a retry (`detcore/src/syscalls/io.rs:564-566`).
  - The deadline is virtual time plus the timeout (`detcore/src/syscalls/io.rs:580-583`).
  - Each iteration takes an `InternalIOPolling` turn, restores the scratch copies of the original sets, re-zeroes the scratch timeout, and injects a probe that carries the snapshotted mask in a scratch wrapper (`detcore/src/syscalls/io.rs:613-662`). A pending signal that the mask unblocks therefore ends the probe with `EINTR` at a point the scheduler chose, not at host arrival time.
  - A scheduler resume with `Signaled` returns `EINTR` (`detcore/src/syscalls/io.rs:643-650`). A non-zero probe result copies the sets back (`detcore/src/syscalls/io.rs:662-672`). Passing the deadline returns 0 with the sets copied (`detcore/src/syscalls/io.rs:674-683`).
  - Every exit writes back `deadline - now` in virtual time and ignores a copy-out fault, as Linux does (`write_pselect6_remaining`, `detcore/src/syscalls/io.rs:705-727`).

Why `model` and not the alternatives:

- `passthrough` would leave a sequentialized thread asleep in the kernel holding the only guest turn. The comment at `detcore/src/syscalls/io.rs:540-548` records the earlier masked path, which ran as blocking external I/O and diverged the execution log of `make -jN`.
- `emulate` cannot answer readiness without the kernel.
- `refuse` would break glibc `select()` and `pselect()`, which almost every event loop uses.

Target (owner decisions 13, 14): every sequentialized wait is a non-blocking probe plus a scheduler wait against a virtual-time deadline. The modelled path already does this (deadline from `thread_observe_time` at `detcore/src/syscalls/io.rs:580-583`, probes at `detcore/src/syscalls/io.rs:613-662`), and its per-probe mask is the shared answer for the masked family: ppoll's `ENOSYS` (`detcore/src/syscalls/io.rs:1039-1048`) is an interim refusal and epoll_pwait's masked kernel wait (`detcore/src/syscalls/io.rs:1217-1221`) is a decision-14 gap, both to be replaced by the same form. The remaining sequentialized gap is `nfds > 64` (`detcore/src/syscalls/io.rs:534-538`): it must also become probe plus wait, with the bitmap read no longer than the kernel's copy (clamped to the descriptor table's `max_fds`, per the comment at `detcore/src/syscalls/io.rs:530-533`). It stays `model`, not `refuse`: glibc `select()` with `FD_SETSIZE` reaches it.

What remains before the code fully realizes `model`:

- **`nfds > 64` in sequentialized run.** That case uses `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:534-538`), whose doc comment (`detcore/src/syscalls/helpers.rs:108-110`) says it is valid only in record/replay or in unsequentialized run. The thread rejoins the schedule when the host kernel returns, so the completion point is host-decided. Callers that pass `FD_SETSIZE` (1024) take this path. That this diverges is a hypothesis from the code, not a measurement; T7 tests it.
- **Record mode with a non-zero timeout** runs as blocking external I/O (`detcore/src/syscalls/io.rs:449`). The recording is faithful, but the point at which the thread rejoins during recording comes from the host.
- The validation probe returns `EIO` if a backend ever accepts the malformed timeout (`detcore/src/syscalls/io.rs:511`). Linux never does this, so it is only a backend guard.

`linux_determinism` stays `nondeterministic`. Hermit removes the timing sources for waits it models; it does not change what Linux does.

## Recommended tests

- T1 (covers F2, F4, F5): under `hermit run --strict --verify` on ptrace, check readiness and copy-out: a readable pipe with a 1 s timeout and a mask returns 1 with only that bit set; a drained pipe with a zero timeout returns 0; a 20 ms timeout returns 0; a raw call with `nfds = 0` writes back a remaining time of 0.
  Existing candidate: `tests/c/pselect6_simulation.c` `run_default_workload` (`tests/c/pselect6_simulation.c:189`), cell `c-programs/pselect6-simulation` (`tests/e2e/manifests/c-programs.yaml:6333`, verify ci and replay ci on ptrace). Also `pselect6_preserves_kernel_abi_and_unblocks_scheduler` at `hermit-cli/tests/pselect6_simulation.rs:30`, which checks the trace for the `Retry #1 ... pselect6(` probe line and then runs `--verify`.
- T2 (covers F3): a NULL-timeout wait on an empty pipe is woken by a second thread, without deadlocking the sequentialized scheduler.
  Existing candidates: the infinite wait in `tests/c/pselect6_simulation.c:389`, and `record_select_null_timeout_woken_by_thread` at `hermit-cli/tests/record_replay.rs:2283`.
- T3 (covers F1, F6, F10): a negative `nfds` gives `EINVAL`; `tv_nsec = 1e9` gives `EINVAL`; an unreadable timeout gives `EFAULT`; an unreadable set gives `EFAULT` with the remaining time still written back.
  Existing candidates: `tests/c/pselect6_simulation.c:235`, `:243`, `:253` and `:281`; `record_select_negative_nfds_preserves_einval` at `hermit-cli/tests/record_replay.rs:2273`.
- T4 (covers F7, F8, F9): the wrapper and mask rules. A NULL inner pointer leaves the mask alone. `ss_len = 4` gives `EINVAL`. An unreadable inner mask gives `EFAULT`. The validation order of F9 holds, including a wrapper split across a page edge.
  Existing candidates: `run_argument_validation_order` (`tests/c/pselect6_simulation.c:134`), `run_wrapper_boundary` (`tests/c/pselect6_simulation.c:67`), and the inaccessible-mask `EFAULT` in `tests/c/signal_delivery_sequence.c:333-348`. None of these asserts `EINVAL` for a wrong `ss_len` on its own.
- T5 (covers F8, F11): a signal whose handler is installed, blocked in the normal mask and unblocked by the temporary mask, sent from another thread during a 1 s wait, gives `EINTR` with a remaining time below 1 s. Run under `--verify`, so the point where the signal lands is identical across runs.
  Existing candidate: the interrupted wait at `tests/c/pselect6_simulation.c:347`.
- T6 (covers F2): `EBADF` for a set bit naming a closed descriptor, in both the modelled path and the zero-timeout path. No existing test asserts this.
- T7 (covers F1, F2): sequentialized `hermit run --verify` of a guest that calls `pselect6` with `nfds = FD_SETSIZE` and a wait that a peer thread ends, repeated in a loop. Record whether the run is deterministic. Record mode already covers this shape (`record_glibc_select_ready_pipe`, `hermit-cli/tests/record_replay.rs:2258`, whose guest runs an `FD_SETSIZE` call), but sequentialized run does not.
- T8 (covers F2, F5, F8): record and replay of ready, zero-timeout, `FD_SETSIZE` and timed-out shapes, with and without a real mask. Replay must reproduce the result, the sets and the remaining time.
  Existing candidates: `record_glibc_select_ready_pipe` (`hermit-cli/tests/record_replay.rs:2258`) and `record_pselect_with_signal_mask_ready_pipe` (`hermit-cli/tests/record_replay.rs:2263`), both driving `tests/c/record_replay_select.c`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/384: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · facts-bot · code_facts changed: tests_naming 2 -> 3.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore replaces the kernel sleep with zero-timeout probes that carry the snapshotted mask, against a virtual-time deadline; record/replay records the outputs); linux_determinism stays nondeterministic; 11 flag behaviours and 8 tests listed; open question about the nfds > 64 path under sequentialized run; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decisions 13 and 14 (nfds > 64 becomes probe plus scheduler wait; the per-probe mask is the shared form for ppoll and epoll_pwait); target_policy unchanged; no new F/T ids; removed both open questions and the heading.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/pselect6_simulation.c, T2 covered by tests/c/pselect6_simulation.c, T8 covered by hermit-cli/tests/record_replay.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pselect6`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pselect6&body=Discussion+for+the+%60pselect6%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpselect6.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pselect6%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pselect6" (https://github.com/rrnewton/hermit/issues/384), legacy variant A1.

The handler carries the review tag `TODO-HUMAN-REVIEW(#686)` (`detcore/src/syscalls/io.rs:455`, `detcore/src/lib.rs:2611`). It names https://github.com/rrnewton/hermit/pull/686, "Determinize pselect6 and expand strict archive coverage".

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=pselect6` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `pselect6` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
pselect6(0, NULL, NULL, NULL, {tv_sec=0, tv_nsec=0}, NULL) = 0 (Timeout)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

The paragraph above is historical (2026-07-24). At hermit f4055eb9cae4, `pselect6` has the typed handler described under Hermit design.
