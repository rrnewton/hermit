---
schema: syscall-pages/v1
syscall: close_range
nr_x86_64: 436
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/341
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+close_range&body=Discussion+for+the+%60close_range%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclose_range.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60close_range%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 5
---

## Summary

`close_range(first, last, flags)` closes, or marks close-on-exec, every open descriptor in an inclusive range, optionally after unsharing the descriptor table. Hermit forwards the `flags == 0` form through record/replay and mirrors the closes in its own descriptor table; it returns `ENOSYS` for every nonzero `flags` value.

## Linux semantics

Added in Linux 5.9. With `flags == 0` the kernel closes each open descriptor in `[first, last]` and skips numbers that are not open; `last` may be `~0U` to mean "every descriptor from `first` up". Closing has the usual side effects: record locks held by the process are released, and the last close of a pipe end, socket or other file wakes the peers. The call cannot block. If the table is shared with other tasks (`CLONE_FILES`), they see the closes too, unless `CLOSE_RANGE_UNSHARE` is given.

### Flags and options

- F1: `flags == 0`, `first <= last`: closes every open descriptor in the inclusive range, skips unopened numbers, and returns 0; `last = ~0U` closes everything from `first` up.
- F2: `first > last`: returns `EINVAL` and closes nothing.
- F3: `CLOSE_RANGE_CLOEXEC` (`1 << 2`): sets `FD_CLOEXEC` on each open descriptor in the range instead of closing it.
- F4: `CLOSE_RANGE_UNSHARE` (`1 << 1`): gives the caller a private copy of a shared descriptor table first, so the closes do not affect other tasks.
- F5: Unknown `flags` bits: return `EINVAL`.
- F6: Shared table without `CLOSE_RANGE_UNSHARE`: the closes, and their side effects (lock release, pipe EOF, socket teardown), are visible to every task that shares the table.

## Determinism analysis

`linux_determinism: conditional`. For a fixed descriptor table the result and the set of closed descriptors are fixed. The variable part is what is in the table when the call runs.

### Nondeterminism sources

- Which descriptors are open in the range depends on what the program and its sibling threads opened before the call; with a shared table that is a race between threads.
- The side effects of the last close (EOF or `EPIPE` at a peer, lock hand-off) reach other processes at a time set by host scheduling.
- Descriptors inherited from the launcher (anything above stdin, stdout and stderr) differ between hosts unless the container fixes the starting table.

The call repeats when the descriptor table and the ordering of threads that share it are fixed.

## Hermit design

Target policy: `model`. Detcore owns the descriptor table, so it must see every close; the kernel performs the closes for the supported form and Detcore mirrors them.

What the code does at hermit f4055eb9cae4:

- Dispatch: a Sysno-guarded arm calls `handle_close_range` because the pinned Reverie exposes the call only as a raw syscall (`detcore/src/lib.rs:2333-2338`).
- Handler: nonzero `flags` returns `ENOSYS` before reaching the kernel (`detcore/src/syscalls/files.rs:1174-1177`), because "CLOSE_RANGE_UNSHARE and CLOSE_RANGE_CLOEXEC need separate shared-table modeling" (`detcore/src/syscalls/files.rs:1159-1163`). Otherwise the call goes through `record_or_replay`; on success Detcore removes the range from its table and releases any port reservation for each closed open file (`detcore/src/syscalls/files.rs:1179-1185`). On failure (for example F2's `EINVAL`) the table is left alone.

Why model: the legacy recommendation was NEEDS-HANDLER, and the handler exists for the common form (glibc's `closefrom` and Python's `subprocess` close descriptors with `flags == 0`). Forwarding without the mirror would leave Detcore's table out of step with the kernel's.

Known gaps:

- F3 and F4 return `ENOSYS` rather than working. Callers commonly treat `ENOSYS` as "close_range not available" and fall back to closing descriptors one by one, which keeps them working, but the guest sees a different result from Linux.
- F5 returns `ENOSYS` where Linux returns `EINVAL` (the 2026-07-24 strace below shows Linux's `EINVAL`).
- F6 is not tested: the mirror runs on the calling thread's state (`detcore/src/syscalls/files.rs:1181`), and I did not establish whether that state is shared with `CLONE_FILES` siblings.

## Recommended tests

- T1 (covers F1): Open descriptors 100 to 103, `close_range(100, 102, 0)` returns 0, 100 to 102 are closed and 103 still reads its file. Existing candidates: `tests/c/close_range_fds.c` (cell `c-programs/close-range-fds`, `tests/e2e/manifests/c-programs.yaml:11256`, verify CI at `tests/e2e/manifests/c-programs.yaml:11274`, backends at `tests/e2e/manifests/c-programs.yaml:11275-11278`), and `tests/c/syscall_quick_wins.c:107-118` driven by the `#[test]` `deterministic_passthrough_syscalls_verify` in `hermit-cli/tests/syscall_quick_wins.rs` (line 30, `--backend=ptrace` at line 90).
- T2 (covers F1, F2): `close_range(5, 4, 0)` returns `EINVAL` and leaves 4 and 5 open; `close_range(3, ~0U, 0)` closes everything above stderr, and a later `open` reuses descriptor 3 under `--verify`.
- T3 (covers F3, F4, F5): `CLOSE_RANGE_CLOEXEC`, `CLOSE_RANGE_UNSHARE` and an unknown bit each return `ENOSYS` under Hermit and leave the table unchanged; this pins today's behaviour until shared-table modeling exists.
- T4 (covers F6): Two threads share the table. A pipe's write end sits at descriptor 45 and its read end at 50; thread A blocks reading descriptor 50 while thread B calls `close_range(40, 49, 0)`. Thread A sees EOF at the same point in every `--verify` run, and afterwards `fcntl(45, F_GETFD)` returns `EBADF` in both threads.
- T5 (covers F1): Record and replay T2; replay reports the same closes and descriptor reuse.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/341#issuecomment-5872955234
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/341: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch sysno-guard.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; close_range records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/341 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (flags == 0 forwarded through record_or_replay and mirrored in Detcore's table; every nonzero flag returns ENOSYS, including CLOEXEC, UNSHARE and unknown bits where Linux gives EINVAL); linux_determinism stays conditional; 6 flag behaviours and 5 tests listed; T1 has ptrace-CI candidates but F2-F6 are uncovered, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/close_range_fds.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `close_range`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+close_range&body=Discussion+for+the+%60close_range%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclose_range.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60close_range%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: close_range" (https://github.com/rrnewton/hermit/issues/341), legacy variant A2.

Legacy summary notes: rank 62 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls. Manual reference: Linux man-pages close_range(2).

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/341.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=close_range -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(436, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
close_range(4294967295, 4294967295, CLOSE_RANGE_UNSHARE|CLOSE_RANGE_CLOEXEC|0xfffffff9) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/341#issuecomment-5872955234
