---
schema: syscall-pages/v1
syscall: set_mempolicy
nr_x86_64: 238
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/438
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_mempolicy&body=Discussion+for+the+%60set_mempolicy%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_mempolicy.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_mempolicy%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_memory_policy_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`set_mempolicy(mode, nodemask, maxnode)` sets the NUMA memory policy that the calling thread's future allocations follow, outside ranges that have their own `mbind` policy. Hermit presents a kernel built without NUMA, so Detcore refuses every `set_mempolicy` with the fixed `ENOSYS` that kernel's stub returns, before it looks at any argument.

Manual reference: Linux man-pages set_mempolicy(2).

## Linux semantics

The policy belongs to the calling thread. Threads and processes created afterwards inherit it, and `execve` keeps it. It applies to page faults in ranges that have no VMA policy of their own. The mode, mode flags and node set have the same meaning and checks as in `mbind`: the node set is read as a bitmap of `maxnode` bits and intersected with the nodes that have memory and the cpuset's `mems_allowed`, and an empty result gives `EINVAL`. There is no range and no flags word.

On a single-node host a valid call changes nothing observable except what `get_mempolicy` later reports.

### Flags and options

- F1: `MPOL_DEFAULT` (0): drop the thread policy and allocate on the local node. `nodemask` must be NULL or empty, otherwise `EINVAL`.
- F2: `MPOL_BIND` (2): allocate only from the given nodes. An empty node set gives `EINVAL`.
- F3: `MPOL_INTERLEAVE` (3) and `MPOL_WEIGHTED_INTERLEAVE` (6, Linux 6.9): spread allocations across the given nodes. An empty node set gives `EINVAL`.
- F4: `MPOL_PREFERRED` (1): prefer one node; an empty node set means local allocation. `MPOL_PREFERRED_MANY` (5, Linux 5.15): prefer a non-empty set of nodes.
- F5: `MPOL_LOCAL` (4, Linux 3.8): allocate on the faulting CPU's node. A non-empty node set gives `EINVAL`.
- F6: mode flags OR'd into `mode`: `MPOL_F_STATIC_NODES` (1<<15) and `MPOL_F_RELATIVE_NODES` (1<<14), which together give `EINVAL`; `MPOL_F_NUMA_BALANCING` (1<<13, Linux 5.12), accepted only with `MPOL_BIND` (and `MPOL_PREFERRED_MANY` on recent kernels), otherwise `EINVAL`.
- F7: invalid mode: a mode at or above `MPOL_MAX` gives `EINVAL`.
- F8: node set: `maxnode` larger than `PAGE_SIZE * 8` bits gives `EINVAL`; an unreadable `nodemask` gives `EFAULT`; a node set with no node that has memory and is in the cpuset gives `EINVAL`.
- F9: scope: the policy is per thread, inherited by `fork` and `clone` children and kept across `execve`; `get_mempolicy` with no flags reports it.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The call does not change what a program computes. Its return value depends on the host: the node count and cpuset decide whether a node set is accepted (F8), and the kernel version decides which modes and mode flags exist (F3 to F6). It repeats on the same host, kernel and cpuset.

### Nondeterminism sources

- Host NUMA topology: the node count and which nodes have memory
- The caller's cpuset `mems_allowed`
- Kernel version, which decides whether `MPOL_LOCAL`, `MPOL_PREFERRED_MANY`, `MPOL_WEIGHTED_INTERLEAVE` and `MPOL_F_NUMA_BALANCING` exist

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `refuse`. Hermit now presents a kernel built without NUMA instead of a single virtual NUMA node. The six memory-policy syscalls are gated by one predicate, `is_memory_policy_syscall` (`detcore/src/syscall_classification.rs:1311`), refused at one dispatch gate by syscall number before the typed match (`detcore/src/lib.rs:7525`), and included in the deterministic-refusal aggregate, so each returns `ENOSYS` before any argument is examined — Linux compiles `mm/mempolicy.o` only under `CONFIG_NUMA`, and its `cond_syscall` entry points are the same stubs. The single-virtual-node model analysed below is superseded: it fabricated node answers a real no-NUMA kernel never gives, and it made `/proc` NUMA views host-shaped work to maintain.

Target policy: `model` (owner decision 7, 2026-10-04); the p3 plan below was `emulate`, and the Target paragraph at the end of this section replaces it. Hermit's contract is a machine with one virtual NUMA node (`detcore/src/lib.rs:2637`, `detcore/src/syscall_classification.rs:318`). The thread policy cannot change placement on that machine, so Detcore synthesizes the result: `Syscall::SetMempolicy(_) => Ok(0)` at `detcore/src/lib.rs:2648`. `set_mempolicy` is in the Determinized list (`detcore/src/syscall_classification.rs:329`), asserted by the unit test at `detcore/src/syscall_classification.rs:2121`.

Why the p3 plan chose `emulate` and not the alternatives:

- `passthrough`, the legacy recommendation, gives host-dependent answers: `MPOL_BIND {1}` succeeds on a two-node host and fails on a one-node host, and newer modes fail on older kernels.
- `model`, in the p3 plan's sense, meant keeping the kernel as the policy store, whose answer is the host-dependent part. Decision 7's `model` keeps the store in Detcore instead (Target paragraph below).
- `refuse` would break programs that set a policy at startup, such as `numactl`-launched programs, libnuma users and some JVM configurations.

What remains before the code fully realizes `emulate` on one virtual node. In each case Linux on a real one-node machine returns an error and Hermit returns 0:

- No argument checking: invalid modes (F7), forbidden mode-flag combinations (F6), a node set without node 0 (F8), a non-empty set with `MPOL_DEFAULT` or `MPOL_LOCAL` (F1, F5), an empty set with `MPOL_BIND`, `MPOL_INTERLEAVE` or `MPOL_PREFERRED_MANY` (F2, F3, F4), and an oversized `maxnode` (F8). The node set is never read, so an unreadable `nodemask` cannot give `EFAULT`.
- No policy memory (F9): after `set_mempolicy(MPOL_BIND, {0})`, `get_mempolicy` still reports `MPOL_DEFAULT` (`detcore/src/syscalls/misc.rs:734`), where Linux reports `MPOL_BIND` and node set `{0}`.

The dispatch carries `TODO-HUMAN-REVIEW(#720)`, which names https://github.com/rrnewton/hermit/pull/720, the batch-3 change that added these arms.

Target (owner decision 7, with decision 6): each guest thread carries a virtual thread memory policy (mode, mode flags and node set) in Detcore's per-thread state, next to the virtual timer slack (`detcore/src/tool_local.rs:1795`). It starts as `MPOL_DEFAULT`, is copied into every fork or clone child as the timer slack is (`detcore/src/lib.rs:1555`; kernel `mpol_dup()` in `copy_process()`), threads created with `CLONE_VM` included, and survives `execve` (F9). A successful `set_mempolicy` replaces the caller's record, `MPOL_DEFAULT` resets it (F1), and `get_mempolicy` with no address flag reports it with its node set (T5). Nothing reaches the host's policy store and placement never changes. The checks follow the kernel's order (set_mempolicy(2); kernel `kernel_set_mempolicy()` then `do_set_mempolicy()` in `mm/mempolicy.c`): (1) `sanitize_mpol_flags()`: a mode at or above `MPOL_MAX`, `MPOL_F_STATIC_NODES` with `MPOL_F_RELATIVE_NODES`, or `MPOL_F_NUMA_BALANCING` with a mode other than `MPOL_BIND` or `MPOL_PREFERRED_MANY` gives `EINVAL` (F6, F7); (2) `get_nodes()`: `maxnode` above `PAGE_SIZE * 8` bits gives `EINVAL` and an unreadable `nodemask` gives `EFAULT` (F8); (3) `mpol_new()` and `mpol_set_nodemask()` on the single virtual node: a non-empty set with `MPOL_DEFAULT` or `MPOL_LOCAL`, an empty set with `MPOL_BIND`, `MPOL_INTERLEAVE`, `MPOL_WEIGHTED_INTERLEAVE` or `MPOL_PREFERRED_MANY`, or a set without node 0 gives `EINVAL` (F1 to F5, F8). An empty set with `MPOL_PREFERRED` is recorded as `MPOL_LOCAL`, as `mpol_new()` does. Today none of this exists: `Syscall::SetMempolicy(_) => Ok(0)` (`detcore/src/lib.rs:2648`).

## Recommended tests

- T1 (covers F1): `set_mempolicy(MPOL_DEFAULT, NULL, 0)` returns 0, and `get_mempolicy` reports `MPOL_DEFAULT` before and after.
  Existing candidate: `tests/c/mempolicy_default.c:65` (cell `c-programs/mempolicy-default`, `tests/e2e/manifests/c-programs.yaml:12449`, verify ci on ptrace, kvm and liteinst). It covers only this mode with a NULL node set.
- T2 (covers F2, F3, F4, F5): each mode with node set `{0}`, or with an empty set for `MPOL_LOCAL` and `MPOL_PREFERRED`, returns 0 under `hermit run --verify` on ptrace, and allocations afterwards still work.
- T3 (covers F6, F7): mode 99, `MPOL_F_STATIC_NODES|MPOL_F_RELATIVE_NODES`, and `MPOL_INTERLEAVE|MPOL_F_NUMA_BALANCING` give `EINVAL`. Today Hermit returns 0.
- T4 (covers F1, F2, F5, F8): `MPOL_BIND {1}`, `MPOL_BIND` with an empty set, `MPOL_DEFAULT {0}`, `MPOL_LOCAL {0}` and `maxnode = PAGE_SIZE * 8 + 2` give `EINVAL`; an unmapped `nodemask` with `MPOL_BIND` gives `EFAULT`.
- T5 (covers F9): after `set_mempolicy(MPOL_BIND, {0})`, `get_mempolicy` in the same thread and in a `fork` child reports `MPOL_BIND` and node set `{0}`; a sibling thread created before the call still reports `MPOL_DEFAULT`. Today Hermit reports `MPOL_DEFAULT` everywhere.
- T6 (covers F6, F7, F8, F9): check order and lifetime. Mode 99 with an unmapped `nodemask` gives `EINVAL` (mode before node set); `MPOL_BIND {1}` with `maxnode = PAGE_SIZE * 8 + 2` gives `EINVAL` from the size check; a thread created with `CLONE_VM` after `set_mempolicy(MPOL_BIND, {0})` reports `MPOL_BIND`, and a program `execve`d afterwards still reports `MPOL_BIND`; a later `set_mempolicy(MPOL_DEFAULT, NULL, 0)` makes the caller report `MPOL_DEFAULT` while that thread keeps `MPOL_BIND`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/438: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (Detcore returns 0 without the kernel, under the single-virtual-NUMA-node contract); linux_determinism stays conditional; related gains https://github.com/rrnewton/hermit/pull/720 and the sibling issues for mbind and get_mempolicy; 9 flag behaviours and 5 tests listed; gaps: no argument checks and no remembered policy; only MPOL_DEFAULT has a test, so tracking_status stays todo; one open question recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (per-thread virtual memory policy read back by get_mempolicy, copied at fork and clone, kept across execve; Linux checks in kernel order); target_policy emulate -> model; new T6; removed the open question on remembering the thread policy
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/mempolicy_default.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: deterministic refusal added (Hermit presents a kernel built without NUMA; all six memory-policy syscalls return ENOSYS); Summary and Hermit design updated, target_policy -> refuse.

## Discussion

[Start a discussion about `set_mempolicy`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_mempolicy&body=Discussion+for+the+%60set_mempolicy%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_mempolicy.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_mempolicy%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: set_mempolicy" (https://github.com/rrnewton/hermit/issues/438), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=set_mempolicy` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `set_mempolicy` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
set_mempolicy(MPOL_DEFAULT, NULL, 0) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
