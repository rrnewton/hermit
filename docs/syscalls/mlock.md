---
schema: syscall-pages/v1
syscall: mlock
nr_x86_64: 149
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/290
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mlock&body=Discussion+for+the+%60mlock%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmlock.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mlock%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`mlock(addr, len)` locks the pages covering `[addr, addr + len)` into RAM: the kernel faults them in now and keeps them resident until they are unlocked or unmapped. Detcore passes the call to the kernel unchanged (`detcore/src/lib.rs:2843`). Its result therefore depends on the host's real `RLIMIT_MEMLOCK`, while Detcore tells the guest that limit is unlimited.

## Linux semantics

Order of checks, from the probes below and upstream `do_mlock`:

- Without `CAP_IPC_LOCK` and with a soft `RLIMIT_MEMLOCK` of 0, every call returns `EPERM`, even with `len == 0`.
- `addr` is rounded down and `addr + len` rounded up to page boundaries. `len == 0` then returns 0.
- Without `CAP_IPC_LOCK` (checked in the initial user namespace, so container root does not have it), the pages already locked plus the new range must fit the soft `RLIMIT_MEMLOCK`, or the call returns `ENOMEM`. Pages that are already locked are not counted twice.
- Each mapping in the range is marked locked, in address order. A hole stops the walk with `ENOMEM`, and the pages before the hole stay locked.
- The pages are then faulted in, which breaks copy-on-write for private writable mappings. A fault failure is reported as `ENOMEM` (for example a `PROT_NONE` mapping, which stays marked locked) or as `EAGAIN` when the kernel cannot get the memory.

Locks do not nest: one `munlock` undoes any number of `mlock` calls on a page. They are not inherited by `fork`, and `execve` and `munmap` remove them. The locked total shows in `/proc/self/status` (`VmLck`) and per mapping in `/proc/self/smaps` (`Locked`, and `lo` in `VmFlags`).

Host results measured for this issue on Linux 7.1.3 as an unprivileged user with a soft limit of 1059890176 bytes: `len == 0` returns 0; a 1-byte lock at `addr + 100` locks 4 kB; a length that wraps returns `ENOMEM`; a range with a hole returns `ENOMEM` with the first page left locked (`VmLck` 4 kB); an unmapped range returns `ENOMEM`; a `PROT_NONE` page returns `ENOMEM` and still raises `VmLck` from 0 to 4 kB; 64 MiB succeeds; locking a page twice and unlocking once leaves `VmLck` 0. With the soft limit at 2 pages, 2 pages succeed and 4 more return `ENOMEM`; with the limit at 0 every call returns `EPERM`.

### Flags and options

- F1: `len == 0`: returns 0 and locks nothing (but see F4 for a zero limit)
- F2: page rounding: the locked range runs from `addr` rounded down to `addr + len` rounded up
- F3: a fully mapped, accessible range: every page is faulted in and locked; copy-on-write is broken for private writable mappings; `VmLck`, smaps `Locked` and the `lo` flag change; returns 0
- F4: the soft `RLIMIT_MEMLOCK` in force, without `CAP_IPC_LOCK` in the initial user namespace: `ENOMEM` when the new total would exceed the soft limit (already-locked pages count once); `EPERM` for every call when the soft limit is 0. Under owner decision 5 the limit in force is the one `getrlimit` and `prlimit64` report to the guest, so these errors come exactly at the reported limit
- F5: `addr + len` wraps: `ENOMEM` when the limit check fails first, `EINVAL` when there is no effective limit, so the errno depends on the limit
- F6: an unmapped range or a range with a hole: `ENOMEM`; pages before the hole remain locked
- F7: a `PROT_NONE` mapping: `ENOMEM`, but the mapping stays marked locked
- F8: the kernel cannot fault the pages in (memory or memory-cgroup pressure): `EAGAIN`
- F9: lifetime: locks do not nest, are not inherited by `fork`, and are removed by `munmap` and `execve`; a range locked on fault by `mlock2(MLOCK_ONFAULT)` becomes fully locked
- F10: mappings the kernel will not lock (`VM_IO`/`VM_PFNMAP` device mappings, hugetlb, DAX, `memfd_secret` memory): skipped without an error and not counted (from upstream source, not probed here)
- F11: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). When the host's real `RLIMIT_MEMLOCK` is below the one the guest was told, the `ENOMEM` (or `EPERM` at a host limit of 0) it causes is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=mlock errno=<ENOMEM|EPERM>`, not handed to the guest

## Determinism analysis

`linux_determinism: conditional`. For a fixed memory map the outcome follows from the guest's own mappings (F1 to F3, F6, F7, F9, F10). Three inputs come from outside the process:

- the soft `RLIMIT_MEMLOCK` inherited from the launching shell, which differs between hosts (F4, F5);
- `CAP_IPC_LOCK` in the initial user namespace (F4, F5);
- host memory and memory-cgroup pressure (F8).

So the same program can lock 16 MiB on one host and get `ENOMEM` on another, and a call near the limit can change result when pressure changes. Locking does not change what a program computes.

### Nondeterminism sources

- The host's real `RLIMIT_MEMLOCK` and `CAP_IPC_LOCK` (F4, F5, F11).
- Host memory and memory-cgroup pressure (F8).

### Cross-host variation

- Host soft `RLIMIT_MEMLOCK` inherited at launch: how much the guest can lock before `ENOMEM`, `EPERM` for every call when it is 0, and whether a wrapping length gives `ENOMEM` or `EINVAL` (F4, F5, F11). The guest is told the limit is unlimited.
- `CAP_IPC_LOCK` of the user running Hermit, in the initial user namespace: with it, the limit does not apply (F4, F5).

## Hermit design

Target policy: `model` (the code at hermit f4055eb9cae4 passes it through).

What the code does today:

- `classify_syscall` returns `PassThrough` for `mlock` in the "ISSUE-REVIEWED PASS-THROUGH SYSCALLS" block (`detcore/src/syscall_classification.rs:816`). The block's comment at `detcore/src/syscall_classification.rs:808` says these remain conditional on Hermit's fixed-container and stable-state assumptions, and `detcore/src/syscall_classification.rs:811` carries `TODO-HUMAN-REVIEW(PR-643)`. The unit test `representative_policies_stay_in_their_reviewed_sections` pins it (`detcore/src/syscall_classification.rs:2105`).
- Dispatch is the generic arm at `detcore/src/lib.rs:2843`, which calls `passthrough` (`detcore/src/lib.rs:367`). Under record and replay, `passthru_opt` (`detcore/src/lib.rs:3140`) leaves it unsubscribed (`detcore/src/lib.rs:1084`), so replay runs it live on the replay host.

Why passthrough is not enough:

- **The guest is told the limit is unlimited.** Detcore's per-process `ResourceLimits` default every limit to `RLIM64_INFINITY` except `RLIMIT_STACK` and `RLIMIT_NOFILE` (`detcore/src/tool_local.rs:279`), so `getrlimit` and `prlimit64` report an unlimited `RLIMIT_MEMLOCK` (`detcore/src/syscalls/sysinfo.rs:163`, `detcore/src/syscalls/sysinfo.rs:222`). The guest cannot change it: any real change to a limit other than `STACK`, `NOFILE` and `CORE` returns `EPERM` (`detcore/src/syscalls/sysinfo.rs:147`). The doc comment at `detcore/src/syscalls/sysinfo.rs:212` says these limits are not enforced on the host.
- **The host still enforces its own limit.** A guest that believes it has no limit gets `ENOMEM` or `EPERM` from the host's real limit (F4, F5), and the threshold differs between hosts. A run and its replay on different hosts can disagree.
- **What Detcore already hides.** `VmLck` in `/proc/self/status` is reported as 0 (`detcore/src/procfs.rs:1372`, rewritten at `detcore/src/procfs.rs:1437`). smaps `Locked` is kept from the host (`detcore/src/procfs.rs:3033`). `mincore` reports every page resident whatever is locked (`handle_mincore`, `detcore/src/syscalls/memory.rs:203`).

Target (owner decision 5, 2026-10-04: limits become real and deterministic): the `RLIMIT_MEMLOCK` the guest reads is the limit in force, so F4 and F5 happen exactly at the reported limit and nowhere else. Hermit gets there in one of two ways, chosen per resource: Detcore enforces the reported limit itself, checking the range against the guest's mappings so F2, F6 and F7 keep Linux's errors and counting locked pages against the reported soft limit; or Hermit configures the guest's host `RLIMIT_MEMLOCK` to the reported value at launch and on each accepted `setrlimit`/`prlimit64`, and the kernel's own check then matches what the guest was told. Either way `EAGAIN` from host pressure (F8) is not handed to the guest.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): `mlock` is unsupported and returns `EPERM` for every call. That is what Linux returns when the soft `RLIMIT_MEMLOCK` is 0 and `CAP_IPC_LOCK` is absent in the initial user namespace: `do_mlock` calls `can_do_mlock` (`mm/mlock.c`) before it rounds the range or walks the mappings, so a hole, a `PROT_NONE` page and `len == 0` all get `EPERM` too (F4). The reported `RLIMIT_MEMLOCK` is 0, soft and hard, so the answer matches what the guest reads, and a raise is `EPERM`, so the guest can never make a lock legal. Detcore answers without calling the host, so nothing is locked physically: smaps `Locked` stays 0 and `lo` never appears, as on Linux after a refused call. Host pressure (F8) and the host's own limit (F11) can no longer reach the guest. Most programs that lock memory treat `EPERM` as the ordinary unprivileged case (gpg, for example, warns about insecure memory and continues); a program that requires the lock fails as it would for an unprivileged user. This settles the mechanism question and replaces the two routes above for `mlock`.

Interim, until `RLIMIT_MEMLOCK` reaches the target (F11): a failure caused by a host limit that the reported limit would not predict is a determinism failure, not a guest failure, and is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=mlock errno=<NAME>` through the infrastructure-error convention. Today it reaches the guest unchanged through the plain passthrough (`detcore/src/syscall_classification.rs:816`, `detcore/src/lib.rs:2843`); that is the gap. A host-limit `ENOMEM` cannot be told apart from a hole `ENOMEM` after the fact, so detecting it needs Detcore's own range check before the call, or the configure route, where a host hard limit below the reported value is found once at launch.

### Gaps and open points

- No test at this SHA calls `mlock` under Hermit.
- Other calls lock memory or charge `RLIMIT_MEMLOCK` too: `mmap` with `MAP_LOCKED` (Linux returns `EPERM` when `can_do_mlock` fails), `shmctl(SHM_LOCK)`, `perf_event_open` buffers and `io_uring` buffer registration. With the reported limit at 0, each must answer as Linux does at a limit of 0, or the host's real limit shows through. Their own bodies own that; this body only records the dependency.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, `mlock(addr, 0)`, `mlock(addr + 100, 1)` and a 64 MiB lock of a private writable mapping all return `EPERM`; smaps shows `Locked: 0 kB` and no `lo` flag for the mapping, the same in both runs. Fails at this SHA, where the passthrough returns 0 on a host with a nonzero limit.
- T2 (covers F4, F5): raw `getrlimit` and `prlimit64` report `RLIMIT_MEMLOCK` as 0 soft and hard; `setrlimit` raising either limit returns `EPERM`; `mlock(addr, 0)` and `mlock` with a wrapping length both return `EPERM`, because the permission check comes first. Fails at this SHA, which reports the limit as unlimited (`detcore/src/tool_local.rs:279`).
- T3 (covers F6, F7): a range with a hole, a fully unmapped range and a `PROT_NONE` page all return `EPERM`, not `ENOMEM`, and smaps afterwards shows no locked page in any of them.
- T4 (covers F8): under a memory cgroup too small for the request, the call still returns `EPERM`: the result does not depend on host pressure. This needs a cgroup the CI host can create.
- T5 (covers F9): `mlock` on a page twice returns `EPERM` both times and `munlock` returns 0; `mlock2(MLOCK_ONFAULT)` on the range returns `EPERM`; a forked child and an `execve`d image show no lock in smaps.
- T6 (covers F10): `mlock` over a `memfd_secret` or hugetlb mapping returns `EPERM`, and smaps shows no `lo` flag on it.
- T7 (covers F11): launch Hermit with `prlimit --memlock=0:0` and again with `--memlock=8192:8192` on the launcher. In both, the guest's 64 KiB `mlock` returns `EPERM`, no `cause=host-rlimit` report appears, and the guest output equals that of a launch under the host's default limit. Today the guest gets the host's answer, which differs between the three launches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/290#issuecomment-5872839500
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/290: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mlock records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/290 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional kept (host RLIMIT_MEMLOCK, CAP_IPC_LOCK and memory pressure decide ENOMEM/EPERM/EAGAIN); target_policy passthrough -> model (Detcore reports RLIMIT_MEMLOCK unlimited at tool_local.rs:279 while the host enforces its real limit); 10 flag behaviours and 6 tests; whether the model still locks physically is an open question for the owner, tracking_status todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host RLIMIT_MEMLOCK, CAP_IPC_LOCK in the initial user namespace); host-transient U1 for the F8 EAGAIN under memory pressure; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (reported RLIMIT_MEMLOCK is the limit in force, enforced by Detcore or configured on the host; interim host-rlimit infrastructure error, passed through today at syscall_classification.rs:816 and lib.rs:2843); target_policy stays model; F4 narrowed to the limit in force; T2 rewritten as a guest-lowered at-limit test; new F11, T7; mechanism question narrowed (option c ruled out); added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (mlock unsupported: EPERM for every call, as can_do_mlock gives at RLIMIT_MEMLOCK 0; reported limit 0 soft and hard; no host call, so no physical lock); target_policy stays model; T1-T7 now assert EPERM, with T7 showing the answer is the same under any host limit; removed host-transient U1 (T4 covers F8); removed open questions 1 (mechanism, now settled) and 2 (the proposal, now approved); new gap on other RLIMIT_MEMLOCK users (mmap MAP_LOCKED, shmctl SHM_LOCK, perf, io_uring)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mlock`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mlock&body=Discussion+for+the+%60mlock%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmlock.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mlock%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mlock" (https://github.com/rrnewton/hermit/issues/290), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/290.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mlock`, and by confirming hermit currently routes `mlock` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mlock(0x406000, 4096)           = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/290#issuecomment-5872839500
