---
schema: syscall-pages/v1
syscall: mlockall
nr_x86_64: 151
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/292
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mlockall&body=Discussion+for+the+%60mlockall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmlockall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mlockall%60%2C+or+in+what+the+page+says%3F%0A
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

`mlockall(flags)` locks every page of the calling process's address space into RAM: the pages mapped now, the pages mapped later, or both. Hermit forwards it to the host kernel unchanged. The kernel then enforces the host's `RLIMIT_MEMLOCK`, while Detcore tells the guest that `RLIMIT_MEMLOCK` is unlimited. The target, approved by the owner on 2026-10-04, is that Detcore reports the limit as 0 and answers `EPERM` itself.

## Linux semantics

`mlockall` acts on the whole memory map (`mm`) of the calling process, so all its threads see the effect. The kernel checks `flags` first. Then `can_do_mlock()` requires a nonzero `RLIMIT_MEMLOCK` or `CAP_IPC_LOCK`. Both this check and the later size check use `capable()`, which tests the capability in the initial user namespace, so root inside a user namespace does not bypass the limit. With `MCL_CURRENT`, the kernel compares the process's total mapped size (`total_vm`) with `RLIMIT_MEMLOCK` and fails with `ENOMEM` if the map is larger and the caller lacks `CAP_IPC_LOCK`. On success it sets the lock bits on every mapping and faults the pages in. Errors during that fault-in are ignored, so the call still returns 0. The call returns 0 or a negative errno and never reports how much memory was locked.

### Flags and options

- F1: `MCL_CURRENT` (1): every current mapping is marked locked and its pages are faulted in before the call returns. `VM_IO`, `VM_PFNMAP` and other special mappings are skipped. Fails with `ENOMEM` when `total_vm` exceeds `RLIMIT_MEMLOCK` and the caller lacks `CAP_IPC_LOCK`.
- F2: `MCL_FUTURE` (2): the process's default mapping flags gain the lock bit, so later `mmap`, `brk`, `mremap` growth and stack growth are locked.
  A later mapping that would push the locked total over `RLIMIT_MEMLOCK` fails: `mmap` returns `EAGAIN`, and `brk` leaves the break unchanged. `MCL_FUTURE` alone does not lock current mappings.
- F3: `MCL_ONFAULT` (4, Linux 4.4): combined with F1 and/or F2, pages are locked as they are first touched instead of being faulted in up front (`VM_LOCKONFAULT`)
- F4: invalid `flags`: 0, any bit outside the three above, or `MCL_ONFAULT` alone: `EINVAL`
- F5: each call replaces the earlier lock mode. A call without `MCL_FUTURE` clears an earlier `MCL_FUTURE`. A call with `MCL_CURRENT` re-marks every current mapping in the new mode, locked up front or on fault.
- F6: `RLIMIT_MEMLOCK` is 0 and the caller lacks `CAP_IPC_LOCK` in the initial user namespace: `EPERM`
- F7: lifetime: locks and the `MCL_FUTURE` default are not inherited by a `fork` child and are dropped by `execve`. `munlock`, `munlockall` and `munmap` remove them.
  Locked memory shows in `/proc/<pid>/status` (`VmLck`) and in `/proc/<pid>/smaps` (`Locked:` and the `lo` mnemonic in `VmFlags`).
- F8: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). When the host's real `RLIMIT_MEMLOCK` is below the one the guest was told, the failure it causes is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>`, not handed to the guest: `ENOMEM` or `EPERM` from `mlockall` itself, or, after `MCL_FUTURE`, `EAGAIN` from a later `mmap` (and the unchanged break from `brk`)

## Determinism analysis

The result is a function of the flags, the size of the process's memory map, the host's `RLIMIT_MEMLOCK` and whether the caller holds `CAP_IPC_LOCK` in the initial user namespace. Under Hermit the map size repeats, because the guest's binaries and mappings are fixed and threads run one at a time. The host limit does not repeat across hosts. It is a host setting that Hermit neither fixes nor reports: on the measurement host used for this body, `ulimit -l` printed 1035049 (KiB, about 1 GiB), while the Linux default is 8 MiB. The call therefore repeats on one host and can differ between hosts.

### Nondeterminism sources

- F1, F6 and F8: the host's `RLIMIT_MEMLOCK` and capabilities decide between success, `ENOMEM` and `EPERM`. A guest with a large map (a JVM, for example) can succeed on one host and fail on another.
- F2 and F8: after `MCL_FUTURE`, every later `mmap`, `brk` and `mremap` can fail when the locked total crosses the host limit, so the host setting reaches the results of other syscalls.
- Physical memory pressure only affects how long the fault-in takes; its errors are ignored and the return value does not change.
- Whether a page is resident is hidden from the guest elsewhere: `mincore` is emulated as all-resident (`detcore/src/syscall_classification.rs:130`-`132` at hermit f4055eb9cae4). `VmLck` in `/proc/<pid>/status` is rewritten to `0 kB` (`detcore/src/procfs.rs:1369`-`1372` and `detcore/src/procfs.rs:1437`-`1441`).
  `/proc/<pid>/smaps`, however, passes `Locked:` and `VmFlags` through unchanged (`detcore/src/procfs.rs:3029`-`3044`). Those two fields show the lock state, and they repeat whenever the call itself repeats.

### Cross-host variation

- Host `RLIMIT_MEMLOCK` inherited at launch (`ulimit -l`): success, `ENOMEM` or `EPERM` from `mlockall` (F1, F6, F8), and after `MCL_FUTURE` the point where later `mmap`, `brk` and `mremap` calls fail (F2, F8). The guest is told the limit is unlimited.
- `CAP_IPC_LOCK` of the user running Hermit, in the initial user namespace: with it, the limit does not apply (F1, F2, F6).

## Hermit design

Target policy: `model` (changed from `investigate` by the decision-5 refinement below). The rendered body carried `passthrough` from the legacy recommendation. The code forwards today, but the current behaviour contradicts Detcore's own virtual resource limits.

Owner decision 5 (2026-10-04: limits become real and deterministic) settles what the guest must see: the `RLIMIT_MEMLOCK` it reads is the limit in force, so F1, F2 and F6 happen exactly at the reported limit. It left the mechanism open, and here the mechanism decided the policy.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): `mlockall` is unsupported. `EINVAL` for invalid `flags` (F4) comes first, as on Linux; every valid `flags` then returns `EPERM`. That is what Linux returns when the soft `RLIMIT_MEMLOCK` is 0 and `CAP_IPC_LOCK` is absent in the initial user namespace: `mlockall` checks `flags`, then `can_do_mlock` (`mm/mlock.c`) (F6). The reported `RLIMIT_MEMLOCK` is 0, soft and hard, and a raise is `EPERM`, so the answer matches what the guest reads and stays that way. Detcore answers without forwarding, so nothing is locked: `smaps` `Locked:` stays 0 and `lo` never appears. `MCL_FUTURE` never takes effect, so F2's effect on later `mmap`, `brk` and `mremap` disappears, and the host's own limit (F8) no longer reaches the guest. Programs that treat a failed `mlockall` as a warning continue; those that treat it as fatal fail as they would for an unprivileged user. This answers the passthrough-versus-emulate question and the `smaps` consistency question.

Interim, until `RLIMIT_MEMLOCK` reaches the target (F8): a failure caused by a host limit that the reported limit would not predict is a determinism failure, not a guest failure, and is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>` through the infrastructure-error convention. Today it reaches the guest unchanged through the plain passthrough (`detcore/src/syscall_classification.rs:818`, `detcore/src/lib.rs:2843`); that is the gap. With the reported limit unlimited, any `ENOMEM` or `EPERM` from `mlockall` after a valid `flags` is one the reported limit would not predict, so `mlockall` itself is easy to detect; the later `EAGAIN` from `mmap` after `MCL_FUTURE` is reported from the `mmap` path.

What the code does at hermit f4055eb9cae4:

- `mlockall` is classified `PassThrough` at `detcore/src/syscall_classification.rs:818`, in the issue-reviewed pass-through block that starts at `detcore/src/syscall_classification.rs:807`.
  That block's comment (`detcore/src/syscall_classification.rs:808`-`809`) says these calls stay "conditional on Hermit's fixed-container and stable-state assumptions". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards the call.
  The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the class at `detcore/src/syscall_classification.rs:2107`. The census list holds it at `detcore/src/syscall_classification.rs:1725`.
- Interception charges a fixed logical cost (`detcore/src/lib.rs:1936`). `mlockall` falls to the `_ => FAST_NS` arm (250 ns) at `detcore/src/syscall_time.rs:156`.
- Code facts show `subscribed_record_replay: false`: under the `--passthru-opt` subscription the call does not reach Detcore at all.
- The conflict: Detcore keeps per-process virtual resource limits. Its default table sets only `RLIMIT_STACK` and `RLIMIT_NOFILE` and leaves every other resource, `RLIMIT_MEMLOCK` included, at `RLIM64_INFINITY` (`detcore/src/tool_local.rs:278`-`293`). `getrlimit` and `prlimit64` answer from that table (`detcore/src/syscalls/sysinfo.rs:163` and `detcore/src/syscalls/sysinfo.rs:222`).
  A guest that wants to change `RLIMIT_MEMLOCK` gets `EPERM`, because only `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_CORE` may change (`detcore/src/syscalls/sysinfo.rs:147`-`151`).
  So a guest is told it may lock unlimited memory, but the forwarded `mlockall` is checked against the host limit. A guest that checks `getrlimit(RLIMIT_MEMLOCK)` and then gets `ENOMEM` (F1), `EPERM` (F6) or a later `EAGAIN` from `mmap` (F2) sees two answers that contradict each other, and which one wins depends on the host.

The alternatives the refinement replaces: `passthrough` with the host limit configured to the reported value, which a launcher holding `CAP_IPC_LOCK` in the initial user namespace still bypasses; and enforcing a nonzero reported limit in Detcore, returning 0 without locking, which would also need `smaps` `Locked:`/`lo` faked and `MCL_FUTURE` enforced on every later `mmap`, `brk` and `mremap`. `EPERM` is not a refusal in the `refuse` sense: it is the ordinary unprivileged answer at a zero limit.

## Recommended tests

- T1 (covers F4): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mlockall, f)` with `f` = 0, 8, `MCL_ONFAULT` alone and `0x80000000`. Assert `EINVAL` each time, and that no mapping in `/proc/self/smaps` gains `lo`.
- T2 (covers F1, F7): in a small C guest under `hermit run --verify` on ptrace, `mlockall(MCL_CURRENT)` returns `EPERM`, and no mapping in `smaps` shows `lo` or a nonzero `Locked:`, before or after a `fork` and an `execve`. `munlockall` returns 0. Fails at this SHA, where the passthrough returns 0 on a host with a large enough limit.
- T3 (covers F2, F5): `mlockall(MCL_FUTURE)` returns `EPERM`; a following `mmap` of a new anonymous region succeeds and shows no `lo`; a `brk` growth succeeds; `mlockall(MCL_CURRENT | MCL_FUTURE)` also returns `EPERM`, and nothing gains `lo`.
- T4 (covers F3): `mlockall(MCL_CURRENT | MCL_ONFAULT)` and `mlockall(MCL_FUTURE | MCL_ONFAULT)` return `EPERM`, and no mapping gains `lo`.
- T5 (covers F8): launch Hermit with `prlimit --memlock=0:0`, again with a limit smaller than the guest's map, and again under the host's default limit. In all three, raw `getrlimit(RLIMIT_MEMLOCK)` reports 0 soft and hard, `mlockall(MCL_CURRENT)` and `mlockall(MCL_FUTURE)` return `EPERM`, a later `mmap` succeeds, no `cause=host-rlimit` report appears, and the guest output is the same. Today the guest gets the host's answer, which differs between the launches.
- T6 (covers F1, F2, F6): raw `getrlimit` and `prlimit64` report `RLIMIT_MEMLOCK` as 0 soft and hard; `setrlimit` raising either limit returns `EPERM`; then every valid `flags` value (`MCL_CURRENT`, `MCL_FUTURE`, both, and each with `MCL_ONFAULT`) returns `EPERM`. Fails at this SHA, which reports the limit as unlimited (`detcore/src/tool_local.rs:278`-`293`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/292#issuecomment-5872843737
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/292: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mlockall records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/292 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional; target_policy passthrough -> investigate because the forwarded call is checked against the host RLIMIT_MEMLOCK while Detcore reports RLIMIT_MEMLOCK as unlimited (detcore/src/tool_local.rs:278-293); listed 7 flag behaviours and 5 tests; tracking_status stays todo pending the owner's passthrough-vs-emulate decision under Open questions
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host RLIMIT_MEMLOCK, CAP_IPC_LOCK in the initial user namespace); no U items, since T5 can set the host limit from the launcher; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (reported RLIMIT_MEMLOCK is the limit in force, configured on the host or enforced by Detcore; interim host-rlimit infrastructure error, passed through today at syscall_classification.rs:818 and lib.rs:2843); target_policy stays investigate (the unchosen mechanism decides passthrough or emulate); T5 now covers F8 and asserts the infrastructure-error report; new F8, T6; the passthrough-vs-emulate question narrowed to the mechanism (keeping the host/reported split is ruled out); added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target_policy investigate -> model (mlockall unsupported: EINVAL for invalid flags first, then EPERM for every valid flags, as can_do_mlock gives at RLIMIT_MEMLOCK 0; Detcore answers without forwarding, so MCL_FUTURE never takes effect); T2-T6 now assert EPERM, no lo, and the same answer under any host limit; T1 unchanged; removed all three open questions (mechanism, smaps consistency, the proposal) and the empty heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mlockall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mlockall&body=Discussion+for+the+%60mlockall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmlockall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mlockall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mlockall" (https://github.com/rrnewton/hermit/issues/292), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/292.

The legacy body said "No effect on computed results; return depends on host RLIMIT_MEMLOCK". This body keeps the second half and corrects the first: after `MCL_FUTURE`, the host limit also decides whether later `mmap`/`brk` calls succeed (F2), so the call can change other syscalls' results.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mlockall`, and by confirming hermit currently routes `mlockall` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mlockall(0)                     = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/292#issuecomment-5872843737
