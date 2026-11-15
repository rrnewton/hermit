---
schema: syscall-pages/v1
syscall: cachestat
nr_x86_64: 451
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/336
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+cachestat&body=Discussion+for+the+%60cachestat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcachestat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60cachestat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_host_kernel_probe_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`cachestat(fd, cstat_range, cstat, flags)` (Linux 6.5 and later) reports how many pages of a file range are in the page cache, dirty, under writeback, evicted, and recently evicted. Hermit refuses it with a fixed `ENOSYS` before anything reaches the host (`detcore/src/lib.rs:2329`), which is what a kernel without the syscall returns, and leaves the caller's `struct cachestat` untouched.

Manual reference: Linux man-pages cachestat(2).

## Linux semantics

The kernel resolves `fd`, reads a `struct cachestat_range { off, len }` from `cstat_range`, converts it to page indices (`len == 0` means from `off` to the end of the file), walks the file's page-cache entries in that range, and writes a `struct cachestat { nr_cache, nr_dirty, nr_writeback, nr_evicted, nr_recently_evicted }` to `cstat`. It returns 0 on success.

`nr_evicted` counts pages that were cached and later reclaimed, read from the shadow entries the page cache leaves behind; `nr_recently_evicted` is the subset that the workingset code would treat as refaults, judged against current memory pressure. For shmem and tmpfs files, pages that have gone to swap are reported as evicted.

The checks run in this order: `fd` first (the host strace under Additional information shows `EBADF` winning over a non-zero `flags`), then the hugetlbfs check, then, on kernels with the 2025 permission fix (Linux 6.14 and later), a write-permission check, then `flags`, then the pointer copies. The permission check mirrors the one `mincore` uses for page-cache side channels: the caller must have the file open for writing, own it or be capable, or be allowed to open it for writing.

The call does not change the page cache; it only observes it.

### Flags and options

- F1: `fd` is not an open descriptor: `EBADF`. This check runs first.
- F2: `fd` refers to a hugetlbfs file: `EOPNOTSUPP`.
- F3: permission check on Linux 6.14 and later: `EPERM` unless the file is open for writing, the caller owns it or has the capability, or the caller could open it for writing. Older kernels skip this check.
- F4: `flags` must be 0; any other value is `EINVAL`.
- F5: range selection: `off` and `len` in bytes, rounded out to whole pages; `len == 0` means through end of file. A range past end of file counts no pages.
- F6: an unreadable `cstat_range` or unwritable `cstat`: `EFAULT`.
- F7: the five output counters `nr_cache`, `nr_dirty`, `nr_writeback`, `nr_evicted` and `nr_recently_evicted`, including the shmem/tmpfs rule that swapped-out pages count as evicted.
- F8: descriptor kinds: any file with a page-cache mapping, including regular files, directories and memfd/shmem files, is accepted; the header of `tests/c/cachestat_refusal.c` records that native Linux succeeds on both a directory and a regular file.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. The counters describe live host page-cache state, which changes without any action by the caller.

### Nondeterminism sources

- Asynchronous reclaim and writeback: pages leave the cache, and dirty pages become clean, on the host's schedule (F7).
- Host memory pressure decides what is evicted and what counts as recently evicted (F7).
- Other processes on the host, and earlier runs, read and write the same files and warm or evict their pages; the result for a file the guest has just written still depends on whether the host has flushed it (F7, F8).
- Host kernel version and patches decide whether the call exists at all and whether the F3 permission check applies.

The call repeats only on a quiescent host with no memory pressure and no other access to the file, which a container cannot guarantee.

## Hermit design

Target policy: `refuse`. The code already does this at hermit f4055eb9cae4:

- `cachestat` is classified `Determinized` (`detcore/src/syscall_classification.rs:554`); the comment at `detcore/src/syscall_classification.rs:544` gives the reason: it exposes mutable host page-cache residency.
- `is_host_kernel_probe_syscall` (`detcore/src/syscall_classification.rs:1277`) matches it, and the dispatch arm at `detcore/src/lib.rs:2329` returns `Err(Error::Errno(Errno::ENOSYS))`. Nothing is forwarded, no argument is read, `cstat` is not written, and the answer does not depend on `--strict` or `--panic-on-unsupported-syscalls`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1334`), hence `refused: true`, so backends that execute syscalls outside Detcore's dispatcher apply the same refusal.

Under Hermit every row of F1 to F8 therefore gives `-1`/`ENOSYS`: a bad `fd`, a non-zero `flags` and a bad pointer included. That matches a kernel from before 6.5, so callers that use `cachestat` as an optimisation already need a fallback.

Why `refuse` and not the alternatives:

- `passthrough` exposes exactly the host state listed under Determinism analysis.
- `emulate` with fixed counters is possible. Hermit already emulates the neighbouring residency probe `mincore` by reporting every page resident (`detcore/src/syscall_classification.rs:131`, dispatched at `detcore/src/lib.rs:2407`). An analogous `cachestat` would report `nr_cache` as the page count of the range clipped to the file size and zero for the other four counters. It would need the file size at the time of the call, and it would claim that every page is cached even for a file the guest never read. The code chose refusal instead; this body keeps that choice, and owner decision 9 settles the consistency question with `mincore` (below).
- `model` would need a virtual page cache, which Hermit does not have.

Target (owner decision 9): `cachestat` stays refused with `ENOSYS`. The decision's rule is to model a family only where a named workload breaks under refusal, and none is known here: callers already need a fallback for kernels before 6.5, which have no `cachestat` at all. The `mincore` emulation (`detcore/src/syscall_classification.rs:131`) is not a reason to emulate `cachestat` too; the two answers do not contradict each other, because a guest that sees `ENOSYS` from `cachestat` sees a kernel without the call. Under owner decision 6, the refusal keeps its one fixed errno before any argument check, which the arm at `detcore/src/lib.rs:2329` already does, so no code change is needed.

What remains is test evidence. The existing tests call `cachestat` with `fd = -1` and a zero range (`tests/c/cachestat_enosys.c:29`), and on a directory and a freshly written regular file with a sentinel-filled result (`tests/c/cachestat_refusal.c:63`, `tests/c/cachestat_refusal.c:79`, `tests/c/cachestat_refusal.c:88`). Non-zero `flags`, bad pointers, a hugetlbfs or shmem file and non-zero ranges are not exercised.

## Recommended tests

- T1 (covers F1, F5): `cachestat(-1, &zero_range, &result, 0)` returns `-1`/`ENOSYS`, not `EBADF`, under `hermit run --strict --verify` on ptrace.
  Existing candidates: `tests/c/cachestat_enosys.c` in cell `c-programs/cachestat-enosys` (`tests/e2e/manifests/c-programs.yaml:451`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:470`), and `host_kernel_probes_fall_back_deterministically` (`hermit-cli/tests/host_kernel_probes.rs:30`), which compiles that guest and runs it with `--backend=ptrace` (`hermit-cli/tests/host_kernel_probes.rs:62`).
- T2 (covers F7, F8): on a directory and on a regular file the guest has just written, `cachestat` returns `ENOSYS` and leaves a sentinel-filled `struct cachestat` byte-identical, so no host counter leaks.
  Existing candidate: `tests/c/cachestat_refusal.c` in cell `c-programs/cachestat-refusal` (`tests/e2e/manifests/c-programs.yaml:11132`, verify `ci: true` with ptrace enabled at `tests/e2e/manifests/c-programs.yaml:11150`).
- T3 (covers F4, F6): `flags = 1`, `flags = 0xffffffff`, a NULL `cstat_range` and a NULL `cstat` on a valid fd all return `ENOSYS`, never `EINVAL` or `EFAULT`.
- T4 (covers F2, F3, F8): `cachestat` on a memfd, on a file opened `O_RDONLY` that the guest cannot write, and, where the host has hugetlbfs mounted, on a hugetlbfs file all return `ENOSYS`, never `EOPNOTSUPP` or `EPERM`.
- T5 (covers F5, F7): a range with non-zero `off`, a `len` that ends past end of file, and `len = 0` from a non-zero `off`, each on a file whose pages were just written and `fsync`ed, all return `ENOSYS` with the result untouched.
- T6 (covers F1, F7): record and replay a guest that calls `cachestat` and branches on the result; replay reproduces `ENOSYS` without a host call. Both existing cells have replay disabled (`tests/e2e/manifests/c-programs.yaml:489`, `tests/e2e/manifests/c-programs.yaml:11164`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/336#issuecomment-5872941963
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/336: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_host_kernel_probe_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; cachestat records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/336 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2329 returns a fixed ENOSYS and never writes the result; counters are live host page-cache state); linux_determinism stays nondeterministic; 8 flag behaviours and 6 tests listed; open question on consistency with the all-resident mincore emulation; no coverage token recorded, so tracking_status stays todo and ptrace_verification untested
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 9 applied: target stays refuse with a fixed ENOSYS before any argument check (detcore/src/lib.rs:2329), because no named workload breaks under refusal; no code change; the mincore consistency open question removed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/cachestat_enosys.c, T2 covered by tests/c/cachestat_refusal.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `cachestat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+cachestat&body=Discussion+for+the+%60cachestat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcachestat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60cachestat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: cachestat" (https://github.com/rrnewton/hermit/issues/336), legacy variant A2. It was ranked 57 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/336.

The dispatch arm and the predicate carry `TODO-HUMAN-REVIEW(PR-847)` tags (`detcore/src/lib.rs:2326`, `detcore/src/syscall_classification.rs:1271`); the human review they ask for has not been recorded in the code.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=cachestat -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(451, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
cachestat(-1, 0xffffffffffffffff, 0xffffffffffffffff, 0xffffffff) = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/336#issuecomment-5872941963
