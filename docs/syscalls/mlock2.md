---
schema: syscall-pages/v1
syscall: mlock2
nr_x86_64: 325
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/291
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mlock2&body=Discussion+for+the+%60mlock2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmlock2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mlock2%60%2C+or+in+what+the+page+says%3F%0A
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

`mlock2(addr, len, flags)` locks the pages of an address range into RAM, either now (`flags == 0`, as `mlock`) or as they are first touched (`MLOCK_ONFAULT`). Hermit forwards it to the host kernel unchanged. Locking does not change memory contents. Whether the call succeeds depends on the host's real `RLIMIT_MEMLOCK` and capabilities, while Detcore shows the guest a virtual, unlimited `RLIMIT_MEMLOCK`.

## Linux semantics

The call is the flagged variant of `mlock`, added in Linux 4.4. The kernel rounds `addr` down and `addr + len` up to page boundaries, then marks every VMA in the range `VM_LOCKED` (with `VM_LOCKONFAULT` for `MLOCK_ONFAULT`). With `flags == 0` it also faults in every page. Locked bytes count against `RLIMIT_MEMLOCK` per process, unless the caller has `CAP_IPC_LOCK` in the initial user namespace. Pages already locked are not counted twice. Locks are not inherited by a `fork` child and are dropped by `execve` and `munmap`. The call writes no memory in the caller.

### Flags and options

- F1: `flags == 0`: lock the range and populate all its pages now, exactly like `mlock`
- F2: `MLOCK_ONFAULT` (1): lock only the pages already resident, and lock the others as they are faulted in. Nothing is populated in advance, and `/proc/self/smaps` `Locked` grows only with touched pages.
- F3: any other bit in `flags` gives `EINVAL`
- F4: range rounding: `addr` need not be page-aligned. `len == 0` succeeds and locks nothing. Locking an already-locked range succeeds without counting it again.
- F5: `ENOMEM`: part of the range is not mapped
- F6: `EPERM`: the soft `RLIMIT_MEMLOCK` in force is 0 and the caller lacks `CAP_IPC_LOCK`. The capability is checked in the initial user namespace, so root inside a user namespace does not count. Under owner decision 5 the limit in force is the one `getrlimit` reports to the guest.
- F7: `EAGAIN`: some pages in the range could not be locked (populate failed) under F1
- F8: mapping types: file-backed, shared, anonymous and stack mappings all lock. `VM_IO`/`VM_PFNMAP` mappings and the vDSO are skipped silently, not refused.
- F9: `ENOMEM`: the new locked total would exceed the soft `RLIMIT_MEMLOCK` in force without `CAP_IPC_LOCK` (under owner decision 5, exactly the reported limit)
- F10: host configuration outside the resource limits: `ENOMEM` when splitting VMAs would exceed the host's `vm.max_map_count`; and when the user running Hermit holds `CAP_IPC_LOCK` in the initial user namespace, the host applies no `RLIMIT_MEMLOCK` check at all
- F11: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). When the host's real `RLIMIT_MEMLOCK` is below the one the guest was told, the `ENOMEM` (or `EPERM` at a host limit of 0) it causes is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=mlock2 errno=<ENOMEM|EPERM>`, not handed to the guest

## Determinism analysis

The memory effect of `mlock2` is invisible to a deterministic program: contents do not change, and residency is not observable through Hermit (see Hermit design). The return value is a function of the guest's arguments and its own address-space layout, plus two pieces of host state: the real `RLIMIT_MEMLOCK` of the guest process and whether it holds `CAP_IPC_LOCK` in the initial namespace. On one host configuration those are fixed, so the call repeats. Across hosts, or across invocations with different `ulimit -l`, the same guest can succeed on one run and get `ENOMEM` or `EPERM` on another.

### Nondeterminism sources

- The host's `RLIMIT_MEMLOCK` for the guest process (F6, F9, F11). It is inherited from whatever launched Hermit; commonly 8 MiB on systemd hosts, smaller on others.
- The host capability set (`CAP_IPC_LOCK` in the initial user namespace) and `vm.max_map_count` (F10).
- The amount already locked by the guest. That is the guest's own state, so it repeats.
- Host memory pressure under F1 (`EAGAIN`, F7), which is rare.
- Latency of populating pages under F1. Virtual time does not see it: Detcore charges every syscall a fixed cost (`detcore/src/lib.rs:1936`). `mlock2` has no entry in the cost table, so it falls back to `detcore/src/syscall_time.rs:156`.

### Cross-host variation

- Host `RLIMIT_MEMLOCK` inherited at launch (`ulimit -l`): how much the guest can lock before `ENOMEM` (F9, F11), and `EPERM` for every call when it is 0 (F6, F11). The guest is told the limit is unlimited.
- `CAP_IPC_LOCK` of the user running Hermit, in the initial user namespace: with it, the limit does not apply (F10).
- Sysctl `vm.max_map_count`: how many VMA splits a lock may cause before `ENOMEM` (F10).

## Hermit design

Target policy: `model` (changed from `passthrough` by the decision-5 refinement below). Owner decision 5 (2026-10-04) requires the `RLIMIT_MEMLOCK` the guest reads to be the limit in force, so F6 and F9 happen exactly at the reported limit. The refinement fixes that limit at 0 and has Detcore answer the call itself, without forwarding it. The call is forwarded today.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): `mlock2` is unsupported. `EINVAL` for unknown flags (F3) comes first, as on Linux; every other call, `MLOCK_ONFAULT` included, returns `EPERM`. That is what Linux returns when the soft `RLIMIT_MEMLOCK` is 0 and `CAP_IPC_LOCK` is absent in the initial user namespace: `mlock2` checks the flags, then `do_mlock` calls `can_do_mlock` (`mm/mlock.c`) before it walks the range (F6). The reported `RLIMIT_MEMLOCK` is 0, soft and hard, and a raise is `EPERM`, so the answer matches what the guest reads and stays that way. Nothing reaches the host, so `smaps` `Locked` stays 0 and host pressure (F7), `vm.max_map_count` and the launcher's `CAP_IPC_LOCK` (F10) and the host's own limit (F11) no longer affect the result. `mlock2` is classified `PassThrough` at `detcore/src/syscall_classification.rs:817`, in the issue-reviewed pass-through block (`detcore/src/syscall_classification.rs:807`). That block's comment says its members "remain conditional on Hermit's fixed-container and stable-state assumptions" (`detcore/src/syscall_classification.rs:809`), and it carries `TODO-HUMAN-REVIEW(PR-643)` at `detcore/src/syscall_classification.rs:811`. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards the call, and `representative_policies_stay_in_their_reviewed_sections` asserts the classification at `detcore/src/syscall_classification.rs:2106`.

Detcore keeps lock state from leaking through the places the guest could read it:

- `/proc/self/status` `VmLck` is in `MEMORY_FIELDS` (`detcore/src/procfs.rs:1372`) and is rewritten to `0 kB` (`detcore/src/procfs.rs:1441`).
- `mincore` is emulated as all-resident (`detcore/src/syscall_classification.rs:132`), so F1 and F2 cannot be told apart through residency.
- `/proc/self/smaps` keeps `Locked` and `VmFlags` verbatim (`detcore/src/procfs.rs:3032`, `detcore/src/procfs.rs:3044`). Those values depend on the guest's own lock calls and page touches, so they repeat on a fixed host.

The inconsistency is the limit. The guest's virtual resource limits default to `RLIM64_INFINITY` for every resource except the stack and the descriptor limit (`detcore/src/tool_local.rs:278`), so `getrlimit(RLIMIT_MEMLOCK)` through `handle_getrlimit` (`detcore/src/syscalls/sysinfo.rs:163`) reports unlimited. Meanwhile the host kernel enforces the real limit on the forwarded `mlock2`. The guest also cannot change its virtual `RLIMIT_MEMLOCK`: `validate_resource_limit_mutation` (`detcore/src/syscalls/sysinfo.rs:131`) refuses changes other than exact no-ops for resources outside the stack, descriptor and core limits. Even accepted changes are not enforced by the host kernel, as the comment on `handle_prlimit64` says (`detcore/src/syscalls/sysinfo.rs:218`). A program that sizes its locked region from `getrlimit` can therefore get `ENOMEM` from a limit it was told does not exist. Owner decision 5 settles this split: the target above removes it.

Interim, until `RLIMIT_MEMLOCK` reaches the target (F11): a failure caused by a host limit that the reported limit would not predict is a determinism failure, not a guest failure, and is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=mlock2 errno=<NAME>` through the infrastructure-error convention. Today the plain passthrough (`detcore/src/syscall_classification.rs:817`, `detcore/src/lib.rs:2843`) hands it to the guest unchanged; that is the gap. A host-limit `ENOMEM` cannot be told apart from a hole `ENOMEM` (F5) after the fact, so detection needs a range check before the call, or the configure route, where a host hard limit below the reported value is found once at launch.

`EPERM` is not a refusal in the `refuse` sense: it is the ordinary answer an unprivileged Linux process gets at a zero limit, and programs that lock memory (databases, crypto libraries locking key material, JIT runtimes) already handle it, usually by continuing without the lock. Returning 0 without locking would be deterministic too, but it would make `smaps` `Locked` disagree with the guest's calls unless that were also faked.

## Recommended tests

- T1 (covers F1, F2, F4, F8): under `hermit run --verify` on ptrace, use raw `syscall(SYS_mlock2, ...)` on small regions. Call it on a 2-page anonymous `mmap` with `flags == 0` from a non-aligned address, a 4-page region with `MLOCK_ONFAULT` after touching one page, a file-backed mapping, and the same range twice. Also call it with `len == 0`. Assert `EPERM` for each, `Locked: 0 kB` for each mapping in `/proc/self/smaps`, and `VmLck: 0 kB` in `/proc/self/status`. Fails at this SHA, where the passthrough returns 0 on a host with a nonzero limit.
  Related existing coverage: `tests/c/syscall_quick_wins.c:57` exercises `munlock` and `munlockall` only; no test names `mlock2`.
- T2 (covers F3): assert `EINVAL` for `flags = 2` and for `flags = 0xffffffff`.
- T3 (covers F5): a range covering an unmapped hole returns `EPERM`, not `ENOMEM`, because the permission check comes before the walk.
- T4 (covers F6, F9): raw `getrlimit` and `prlimit64` report `RLIMIT_MEMLOCK` as 0 soft and hard; `setrlimit` raising either limit returns `EPERM`; `mlock2(addr, 4096, 0)` and `mlock2(addr, 4096, MLOCK_ONFAULT)` return `EPERM`. Fails at this SHA, which reports the limit as unlimited (`detcore/src/tool_local.rs:278`).
- T5 (covers F11): launch Hermit with `prlimit --memlock=0:0` and again with `--memlock=8192:8192` on the launcher. In both, the guest's 64 KiB `mlock2` with `flags == 0` returns `EPERM`, no `cause=host-rlimit` report appears, and the guest output equals that of a launch under the host's default limit. Today the guest gets the host's answer, which differs between the three launches.
- T6 (covers F7, F10): the result is `EPERM` whatever the host state: under a memory cgroup too small for the request (which gives `EAGAIN` natively), with a range split into more VMAs than a lowered `vm.max_map_count` allows, and with a launcher that holds `CAP_IPC_LOCK` (which bypasses the limit natively). Each case needs host setup the CI host must provide.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/291#issuecomment-5872841591
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/291: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mlock2 records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/291 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (classified at detcore/src/syscall_classification.rs:817; success depends on the host RLIMIT_MEMLOCK and CAP_IPC_LOCK); 8 flag behaviours and 3 tests listed, no existing test names mlock2; an open question (the guest sees an unlimited virtual RLIMIT_MEMLOCK while the host enforces the real one) keeps tracking_status todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host RLIMIT_MEMLOCK, CAP_IPC_LOCK, vm.max_map_count); F5 narrowed to the unmapped-hole ENOMEM; new F9, the host-limit ENOMEM; host-config U2 for F6 and F9, host-transient U1 for F7; T3 now covers F5 only; the open question on covering the limit paths is resolved by decision 1
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (reported RLIMIT_MEMLOCK is the limit in force, enforced by Detcore or configured on the host; interim host-rlimit infrastructure error, passed through today at syscall_classification.rs:817 and lib.rs:2843); target_policy stays passthrough (the configure route keeps the call forwarded); F6 and F9 narrowed to the limit in force; U2 reworked to cover only the new host-config F10 (vm.max_map_count, CAP_IPC_LOCK); new F10, F11, T4, T5; removed the virtual-versus-host RLIMIT_MEMLOCK question; added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target_policy passthrough -> model (mlock2 unsupported: EINVAL for unknown flags first, then EPERM for every call, as can_do_mlock gives at RLIMIT_MEMLOCK 0; Detcore answers without forwarding); T1, T3 and T4 now assert EPERM; T5 shows the answer is the same under any host limit; host-transient U1 and host-config U2 replaced by new T6 (EPERM regardless of host pressure, vm.max_map_count and CAP_IPC_LOCK); removed the standing-policy proposal and the empty Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mlock2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mlock2&body=Discussion+for+the+%60mlock2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmlock2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mlock2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mlock2" (https://github.com/rrnewton/hermit/issues/291), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/291.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mlock2`, and by confirming hermit currently routes `mlock2` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mlock2(0x406000, 4096, 0)       = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/291#issuecomment-5872841591
