---
schema: syscall-pages/v1
syscall: syncfs
nr_x86_64: 306
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/493
  - https://github.com/rrnewton/hermit/issues/514
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+syncfs&body=Discussion+for+the+%60syncfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsyncfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60syncfs%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`syncfs(fd)` writes out all dirty data and metadata of the filesystem that contains the open file `fd`, and waits for it to finish. Hermit forwards it to the host kernel unchanged. The guest sees 0 or an errno, and that result does not depend on timing.

Manual reference: [syncfs(2)](https://man7.org/linux/man-pages/man2/sync.2.html).

## Linux semantics

The call takes any descriptor on the target filesystem, including a read-only one or a directory. The kernel looks up the superblock of the file's mount and runs `sync_filesystem` on it, which writes back inodes and data and calls the filesystem's `->sync_fs`. Since Linux 5.8 it also returns any writeback error recorded on the superblock since this descriptor was opened. The call blocks until the writeback finishes, and it writes no memory in the caller.

### Flags and options

- F1: valid fd on a disk-backed, writable filesystem: flush the whole filesystem and return 0
- F2: valid fd on a filesystem with nothing to write back (tmpfs, procfs, sysfs, a pipe or socket's pseudo-filesystem) or on a read-only mount: return 0 without doing any I/O
- F3: a bad fd, a closed fd, or an `O_PATH` fd: `EBADF`
- F4: writeback errors (Linux 5.8+): if any writeback on the superblock failed since `fd` was opened, the call returns that error (typically `EIO` or `ENOSPC`) once per descriptor. The error can come from a write by another process on the same filesystem, including one outside the container. `EDQUOT` and other filesystem-specific `->sync_fs` errors can also appear.

## Determinism analysis

The result depends only on the descriptor (F2, F3) and on the host storage's health (F4). Dirty state and concurrent writers change how long the call takes, not what it returns. The call repeats whenever the guest's files are on the same kind of filesystem and the storage does not fail.

### Nondeterminism sources

- Writeback errors (F4). They come from the host storage and can be caused by writers outside the container. Like other host storage failures in this set (see mkdir), they are a nondeterminism source but do not change the rating.
- Latency, which grows with the amount of dirty data on the whole filesystem, including other processes' data. The guest does not see it in virtual time: Detcore charges every syscall a fixed cost (`detcore/src/lib.rs:1936`). `syncfs` has no entry in the cost table, so it falls back to `detcore/src/syscall_time.rs:156`.

## Hermit design

Target policy: `passthrough`. The code already does this. `syncfs` is classified `PassThrough` at `detcore/src/syscall_classification.rs:828`, in the issue-reviewed pass-through block (`detcore/src/syscall_classification.rs:807`). That block's comment at `detcore/src/syscall_classification.rs:809` says its members "remain conditional on Hermit's fixed-container and stable-state assumptions", and it carries `TODO-HUMAN-REVIEW(PR-643)` at `detcore/src/syscall_classification.rs:811`. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards the call. `representative_policies_stay_in_their_reviewed_sections` asserts the classification at `detcore/src/syscall_classification.rs:2117`.

Why passthrough, not `emulate` (always return 0):

- The call has no effect the guest can observe except its errno.
- An emulation would hide real F4 writeback errors from programs that check `syncfs` for durability, such as databases and package managers, which would be a correctness loss with no determinism gain under Hermit's fixed-storage assumption.
- Its sibling `sync` sits next to it in the same block (`detcore/src/syscall_classification.rs:827`).

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, write and `fsync` a file in a scratch directory. Call raw `syscall(SYS_syncfs, fd)` on that file's descriptor, on an `O_RDONLY` descriptor of its directory, on a `/proc/self/status` descriptor and on one end of a `pipe`. Assert 0 for each.
  Existing partial coverage: `tests/c/fsync_durability.c:57` calls `syncfs(fd)` through the glibc wrapper on a written file and expects 0 (cell `c-programs/fsync-durability`, `tests/e2e/manifests/c-programs.yaml:11947`, verify ci on ptrace).
- T2 (covers F3): assert `EBADF` for fd `-1`, for a closed fd and for an `O_PATH` fd.
- T3 (covers none): call `syncfs` twice on a healthy filesystem and assert 0 both times, so the error-reporting path at least runs under `--verify`. It produces no writeback error, so it does not cover F4, which U1 records.
- U1 (covers F4; host-transient): writeback errors come from host storage health, free space and quotas, and can be caused by writers outside the container. The plain passthrough (`detcore/src/syscall_classification.rs:828`, `detcore/src/lib.rs:2843`) hands them to the guest today. F4 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/493: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/514 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (classified at detcore/src/syscall_classification.rs:828, forwarded at detcore/src/lib.rs:2843); merged duplicate https://github.com/rrnewton/hermit/issues/514, whose CONDITIONAL rating lost because its sources change only latency or are host storage failures; 4 behaviours and 3 tests listed; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant stays false (writeback errors are transient host state, and a healthy host returns 0 on every kernel); F4 recorded as host-transient (U1); T3 no longer claims F4; the open question on storage fault injection is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `syncfs`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+syncfs&body=Discussion+for+the+%60syncfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsyncfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60syncfs%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: syncfs" (https://github.com/rrnewton/hermit/issues/493), legacy variant A1.

Duplicate https://github.com/rrnewton/hermit/issues/514 (filed 2026-07-24 as "Syscall classification: syncfs", legacy variant A1) is merged into this body. Its comment history (0 comments) stays on that issue, and this issue's comment history stays on https://github.com/rrnewton/hermit/issues/493.

### Merge reconciliation

- Classification: https://github.com/rrnewton/hermit/issues/493 said deterministic and https://github.com/rrnewton/hermit/issues/514 said CONDITIONAL. Deterministic won. The duplicate's sources are filesystem-wide dirty state, delayed writeback errors, storage health and concurrent writers. Dirty state and concurrent writers change latency, which virtual time does not see. Writeback errors and storage health are host failures (F4), which this set records as nondeterminism sources without changing the rating.
- The duplicate's point that modern Linux reports writeback errors recorded for the filesystem is kept as F4. Its note that Hermit's forwarding is consistent with the documented filesystem limitation is kept in Hermit design.
- Both issues recommended PASS-THRU, and the code agrees.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/493)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=syncfs` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `syncfs` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
syncfs(4)                       = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/514)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=syncfs`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `syncfs` in the UNCLASSIFIED temporary-passthrough block. Flushing the filesystem containing /tmp succeeded.

Observed strace line:
```text
syncfs(3)                       = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
