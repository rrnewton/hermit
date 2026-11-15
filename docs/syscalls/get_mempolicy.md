---
schema: syscall-pages/v1
syscall: get_mempolicy
nr_x86_64: 239
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/370
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+get_mempolicy&body=Discussion+for+the+%60get_mempolicy%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fget_mempolicy.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60get_mempolicy%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_memory_policy_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`get_mempolicy(policy, nodemask, maxnode, addr, flags)` reports a NUMA memory policy: the calling thread's, or the one governing an address. Hermit presents a kernel built without NUMA, so Detcore refuses every `get_mempolicy` with the fixed `ENOSYS` that kernel's stub returns, before it looks at any argument. Library feature probes such as libnuma's therefore report NUMA unavailable, exactly as on such a kernel.

Manual reference: Linux man-pages get_mempolicy(2).

## Linux semantics

`flags` selects what is reported. The policy and node set come from the thread's policy, or from the VMA containing `addr` when `MPOL_F_ADDR` is set. `*policy` receives the mode OR'd with its mode flags, or a node number for `MPOL_F_NODE`. `nodemask`, when not NULL, receives a bitmap of `maxnode` bits; for `MPOL_DEFAULT` that bitmap is all zeros. Every answer comes from host state: the node count, the cpuset, and where the kernel happened to place a page.

### Flags and options

- F1: `flags == 0`, `addr == 0`: report the calling thread's policy mode, with its mode flags, in `*policy`, and its node set in `*nodemask`. A fresh process reports `MPOL_DEFAULT` and an empty node set.
- F2: `MPOL_F_ADDR` (2): report the policy of the VMA containing `addr`, falling back to the thread policy where the VMA has none. An `addr` in no VMA gives `EFAULT`.
- F3: `MPOL_F_NODE|MPOL_F_ADDR` (3): report in `*policy` the node holding the page at `addr`, faulting it in first if needed.
- F4: `MPOL_F_NODE` (1) alone: if the thread policy is `MPOL_INTERLEAVE`, report the node the next interleaved allocation will use; with any other policy, `EINVAL`.
- F5: `MPOL_F_MEMS_ALLOWED` (4, Linux 2.6.24): report the nodes the thread may use, its cpuset's `mems_allowed`, in `*nodemask`, and 0 in `*policy`. Combined with `MPOL_F_NODE` or `MPOL_F_ADDR`, `EINVAL`.
- F6: invalid `flags`: any bit other than the three above gives `EINVAL`. A non-zero `addr` without `MPOL_F_ADDR` gives `EINVAL`.
- F7: node set size: a non-NULL `nodemask` with `maxnode` smaller than the number of possible node IDs gives `EINVAL`; a `maxnode` larger than `PAGE_SIZE * 8` bits gives `EINVAL`.
- F8: output pointers: NULL `policy` or NULL `nodemask` skips that output; an unwritable pointer gives `EFAULT`.
- F9: a policy set earlier through `set_mempolicy` or `mbind` is reported, including its mode flags and node set.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. The node a page lands on (F3) depends on which CPU the faulting thread ran on and on memory pressure. The allowed-node set (F5) is the host cpuset. The `EINVAL` for a small `maxnode` (F7) depends on the host's possible node count. On a single-node host most answers are fixed, but the call has no host-independent answer in general.

### Nondeterminism sources

- Host NUMA topology and cpusets, read by F5 and F7
- Concurrent policy changes and page migration by other threads, or by the kernel's automatic NUMA balancing, which change F2 and F3
- Address mapping state, which decides `EFAULT` for F2 and F3 and where a page was placed

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `refuse`. Hermit now presents a kernel built without NUMA instead of a single virtual NUMA node. The six memory-policy syscalls are gated by one predicate, `is_memory_policy_syscall` (`detcore/src/syscall_classification.rs:1311`), refused at one dispatch gate by syscall number before the typed match (`detcore/src/lib.rs:7525`), and included in the deterministic-refusal aggregate, so each returns `ENOSYS` before any argument is examined — Linux compiles `mm/mempolicy.o` only under `CONFIG_NUMA`, and its `cond_syscall` entry points are the same stubs. The single-virtual-node model analysed below is superseded: it fabricated node answers a real no-NUMA kernel never gives, and it made `/proc` NUMA views host-shaped work to maintain.

Target policy: `model` (owner decision 7, 2026-10-04); the p3 plan below was `emulate`, and the Target paragraph at the end of this section replaces it. Hermit's contract is a machine with one virtual NUMA node (`detcore/src/lib.rs:2637`). The handler's doc comment says every address resolves to node 0 and the result is "fully emulated (never injected)" (`detcore/src/syscalls/misc.rs:719`). The dispatch is `detcore/src/lib.rs:2649`. The handler at `detcore/src/syscalls/misc.rs:726` writes 0 to `*policy` if the pointer is given (`detcore/src/syscalls/misc.rs:734`) and returns 0. `get_mempolicy` is in the Determinized list (`detcore/src/syscall_classification.rs:330`), asserted by the unit test at `detcore/src/syscall_classification.rs:2121`.

The legacy recommendation was NEEDS-HANDLER, and the handler now exists. The p3 plan chose `emulate` because every Linux answer is host state that Hermit replaces with the fixed one-node machine. Passing the call to the kernel would leak the host's topology and cpuset; `refuse` would break libnuma's `numa_available()` probe and glibc or runtime code that queries the policy.

With 0 in `*policy`, the handler already gives the one-node answer for F1 (`MPOL_DEFAULT`), F2 (`MPOL_DEFAULT` for every range), F3 (node 0) and the policy half of F5.

What remains before the code fully realizes `emulate` on one virtual node:

- **The node set is never written** (F1, F5). The comment at `detcore/src/syscalls/misc.rs:731` explains that reverie types `nodemask` as an immutable pointer (`Option<Addr<c_ulong>>` in reverie-syscalls at the pin `d766df20b780`), and that `MPOL_DEFAULT` has no node set. Linux still writes it: all zeros for F1, and `{0}` for `MPOL_F_MEMS_ALLOWED` (F5). Under Hermit the caller's buffer keeps whatever it held. libnuma reads `MPOL_F_MEMS_ALLOWED` to build its allowed-node mask, so an uninitialized buffer can give it an arbitrary node set.
- **No argument checking.** Unknown flag bits, `MPOL_F_MEMS_ALLOWED` combined with another flag, and `addr` without `MPOL_F_ADDR` (F5, F6) return 0 instead of `EINVAL`. `MPOL_F_NODE` alone (F4) returns 0 and node 0 instead of `EINVAL`. `maxnode` is not checked (F7). `MPOL_F_ADDR` on an unmapped address (F2) returns 0 instead of `EFAULT`. One consequence: the 2026-07-24 host probe below, with every argument `-1`, gives `EINVAL` on Linux; by this reading Hermit tries to write 0 to address `-1` and returns `EFAULT`. That is a hypothesis from the code, not a measurement.
- **No policy memory** (F9). `set_mempolicy` and `mbind` return 0 without recording anything (`detcore/src/lib.rs:2647`, `:2648`), so this call reports `MPOL_DEFAULT` even after the guest set `MPOL_BIND`.

The handler carries `TODO-HUMAN-REVIEW(#720)` (`detcore/src/syscalls/misc.rs:725`), which names https://github.com/rrnewton/hermit/pull/720, the batch-3 change that added it.

`linux_determinism` stays `nondeterministic`; it describes Linux, and Hermit's emulation removes every source listed above.

Target (owner decision 7, with decision 6): `get_mempolicy` reports the virtual policies that the set_mempolicy body (per-thread record in Detcore's thread state) and the mbind body (per-range record in `MemoryMetadata`, `detcore/src/memory.rs:54`) add, on the same one-node machine; nothing is read from the host. The checks and outputs follow the kernel's order (get_mempolicy(2); kernel `kernel_get_mempolicy()` then `do_get_mempolicy()` in `mm/mempolicy.c`): (1) a non-NULL `nodemask` with `maxnode` below the virtual machine's node-ID count, 1, gives `EINVAL` (F7); (2) a `flags` bit outside `MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED` gives `EINVAL` (F6); (3) `MPOL_F_MEMS_ALLOWED` combined with another flag gives `EINVAL`, and alone reports policy 0 and node set `{0}` (F5); (4) with `MPOL_F_ADDR`, an `addr` in no mapping gives `EFAULT`, and the policy is the range record at `addr`, or the thread record where the range has none (F2); without it, a non-zero `addr` gives `EINVAL` (F6); (5) with `MPOL_F_NODE` and `MPOL_F_ADDR`, `*policy` is node 0 (F3); with `MPOL_F_NODE` alone, a thread record of `MPOL_INTERLEAVE` (or `MPOL_WEIGHTED_INTERLEAVE`) reports node 0, the only node, and any other thread policy gives `EINVAL` (F4); otherwise `*policy` is the recorded mode OR'd with its mode flags, `MPOL_DEFAULT` when nothing is recorded (F1, F9); (6) `*policy` is written, an unwritable pointer giving `EFAULT` (F8); (7) `nodemask` is written with the recorded node set (the set as given for `MPOL_F_STATIC_NODES` or `MPOL_F_RELATIVE_NODES`, otherwise its intersection with `{0}`), all zeros for `MPOL_DEFAULT` and `MPOL_LOCAL`, zero-filled to `maxnode` bits, `EINVAL` when that exceeds a page and `EFAULT` when unwritable (F1, F5, F7, F8, F9). Writing `nodemask` needs a writable pointer type in reverie-syscalls, or a cast in the handler, since the field is typed immutable today (`detcore/src/syscalls/misc.rs:731`). The address check in step 4 needs the guest's mapping list, the same need as `mbind`'s hole check. Today the handler writes 0 to `*policy` and returns 0 whatever the arguments (`detcore/src/syscalls/misc.rs:734`).

## Recommended tests

- T1 (covers F1, F2, F8): with no flags, and with `MPOL_F_ADDR` on a mapped address, `*policy` is `MPOL_DEFAULT`; NULL `nodemask` is accepted.
  Existing candidates: `tests/c/mempolicy_default.c:55` and `:59` (cell `c-programs/mempolicy-default`, `tests/e2e/manifests/c-programs.yaml:12449`, verify ci on ptrace, kvm and liteinst), and `tests/c/numa_node_identity.c:83` (cell `c-programs/numa-node-identity`, `tests/e2e/manifests/c-programs.yaml:12704`, verify ci on ptrace, kvm and liteinst). Neither passes a node set or a NULL `policy`.
- T2 (covers F1, F5, F7): pre-fill a 1024-bit `nodemask` with `0xff` bytes and call with no flags; Linux zeroes it. Call again with `MPOL_F_MEMS_ALLOWED`; Linux on one node writes exactly bit 0. Both runs of `hermit run --verify` on ptrace must print the same mask, and it must match Linux. Also call with `maxnode = 0` and a non-NULL `nodemask`, which Linux rejects with `EINVAL`.
- T3 (covers F3): `MPOL_F_NODE|MPOL_F_ADDR` on a touched page and on an untouched page reports node 0 in both cases.
- T4 (covers F2, F4, F6): `EINVAL` for `flags = 8`, for `MPOL_F_MEMS_ALLOWED|MPOL_F_ADDR`, for a non-zero `addr` with no flags, and for `MPOL_F_NODE` alone under the default policy; `EFAULT` for `MPOL_F_ADDR` on an `munmap`ped address. Today Hermit returns 0 for all of these.
- T5 (covers F8): a NULL `policy` with a valid `nodemask` succeeds; an unwritable `policy` gives `EFAULT`.
- T6 (covers F9): after `set_mempolicy(MPOL_BIND, {0})`, `get_mempolicy` reports `MPOL_BIND` and `{0}`, as on Linux. Today Hermit reports `MPOL_DEFAULT`.
- T7 (covers F2, F4, F9): range and interleave read-back. After `mbind(MPOL_BIND, {0})` on one page of a two-page mapping, `MPOL_F_ADDR` reports `MPOL_BIND` and `{0}` for that page and the thread policy for the other; after `set_mempolicy(MPOL_INTERLEAVE, {0})`, `MPOL_F_NODE` alone reports node 0 instead of `EINVAL`; a mode flag set with the policy (`MPOL_BIND | MPOL_F_STATIC_NODES`) is reported OR'd into `*policy`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/370: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (Detcore writes MPOL_DEFAULT or node 0 to *policy and returns 0 without the kernel, under the single-virtual-NUMA-node contract); linux_determinism stays nondeterministic; related gains https://github.com/rrnewton/hermit/pull/720 and the sibling issues for mbind and set_mempolicy; 9 flag behaviours and 6 tests listed; gaps: the node set is never written (including MPOL_F_MEMS_ALLOWED), no flag, addr or maxnode checks, and no remembered policy; two existing cells cover only the mode query, so tracking_status stays todo; one open question recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (reports the virtual thread and range policies recorded by set_mempolicy and mbind, writes the node set, MPOL_F_NODE alone answers node 0 under a recorded interleave policy; Linux checks in kernel order); target_policy emulate -> model; new T7; removed the open question on remembering policies
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/mempolicy_default.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: deterministic refusal added (Hermit presents a kernel built without NUMA; all six memory-policy syscalls return ENOSYS); Summary and Hermit design updated, target_policy -> refuse.

## Discussion

[Start a discussion about `get_mempolicy`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+get_mempolicy&body=Discussion+for+the+%60get_mempolicy%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fget_mempolicy.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60get_mempolicy%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: get_mempolicy" (https://github.com/rrnewton/hermit/issues/370), legacy variant A2. It was ranked 91 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=get_mempolicy -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(239, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
get_mempolicy(0xffffffffffffffff, 0xffffffffffffffff, 18446744073709551615, 0xffffffffffffffff, MPOL_F_NODE|MPOL_F_ADDR|MPOL_F_MEMS_ALLOWED|0xfffffffffffffff8) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
