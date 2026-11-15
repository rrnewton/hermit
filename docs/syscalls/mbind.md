---
schema: syscall-pages/v1
syscall: mbind
nr_x86_64: 237
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/282
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mbind&body=Discussion+for+the+%60mbind%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmbind.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mbind%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_memory_policy_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`mbind(addr, len, mode, nodemask, maxnode, flags)` sets a NUMA memory policy for a range of the calling process's address space, and can move pages already allocated there. Hermit presents a kernel built without NUMA, so Detcore refuses every `mbind` with the fixed `ENOSYS` that kernel's stub returns, before it looks at any argument. Memory-policy-aware programs take their no-NUMA fallback, exactly as on such a kernel.

Manual reference: Linux man-pages mbind(2).

## Linux semantics

The kernel attaches a policy, made of a mode, optional mode flags and a node set, to every VMA in `[addr, addr+len)`, splitting VMAs at the boundaries. The policy decides which node future page faults in the range allocate from. With `MPOL_MF_MOVE` or `MPOL_MF_MOVE_ALL`, pages already in the range that violate the new policy are migrated.

The node set is read from `nodemask` as a bitmap of `maxnode` bits. The kernel intersects it with the nodes that have memory and with the caller's cpuset `mems_allowed`. If nothing is left, the call fails with `EINVAL`. On a single-node host, a valid call is inert: every page is already on node 0, and only node 0 is allowed.

Errors are checked roughly in this order: the flags word, the range, the mode and mode flags, the node set, then the VMAs in the range.

### Flags and options

- F1: `MPOL_DEFAULT` (0): remove the range's policy so the thread's policy applies. `nodemask` must be NULL or empty, otherwise `EINVAL`. `MPOL_MF_STRICT` is ignored with this mode.
- F2: `MPOL_BIND` (2): allocate only from the given nodes. An empty node set gives `EINVAL`.
- F3: `MPOL_INTERLEAVE` (3) and `MPOL_WEIGHTED_INTERLEAVE` (6, Linux 6.9): spread allocations across the given nodes, page by page or by weight. An empty node set gives `EINVAL`.
- F4: `MPOL_PREFERRED` (1): prefer one node; an empty node set means local allocation. `MPOL_PREFERRED_MANY` (5, Linux 5.15): prefer a set of nodes, which must not be empty.
- F5: `MPOL_LOCAL` (4, Linux 3.8): allocate on the faulting CPU's node. A non-empty node set gives `EINVAL`.
- F6: mode flags OR'd into `mode`: `MPOL_F_STATIC_NODES` (1<<15) and `MPOL_F_RELATIVE_NODES` (1<<14), which together give `EINVAL`; `MPOL_F_NUMA_BALANCING` (1<<13), accepted only with `MPOL_BIND` (and `MPOL_PREFERRED_MANY` on recent kernels), otherwise `EINVAL`.
- F7: invalid mode: a mode at or above `MPOL_MAX` gives `EINVAL`.
- F8: node set: `maxnode` larger than `PAGE_SIZE * 8` bits gives `EINVAL`; an unreadable `nodemask` gives `EFAULT`; a node set with no node that has memory and is in the cpuset gives `EINVAL`.
- F9: `MPOL_MF_STRICT` (1): with no move flag, fail with `EIO` if an existing page in the range is not on an allowed node. With a move flag, fail with `EIO` if a page could not be moved.
- F10: `MPOL_MF_MOVE` (2): migrate existing pages mapped only by this process. `MPOL_MF_MOVE_ALL` (4): migrate shared pages too, which needs `CAP_SYS_NICE`, otherwise `EPERM`. Any other bit in `flags` gives `EINVAL`.
- F11: range: an `addr` that is not page aligned gives `EINVAL`; `len` is rounded up to whole pages; `len == 0` returns 0; a wrapping range gives `EINVAL`; a hole in the range gives `EFAULT`; kernel memory exhaustion gives `ENOMEM`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. NUMA placement never changes the bytes a program computes. What changes across hosts is the return value: the number of nodes and the cpuset decide whether a node set is accepted (F8), the kernel version decides which modes exist (F3, F4, F5, F6), and privilege decides `MPOL_MF_MOVE_ALL` (F10). Page placement at call time decides `EIO` under `MPOL_MF_STRICT` (F9). The call repeats on one host, with one cpuset, at one privilege level, when no other process migrates the pages.

### Nondeterminism sources

- Host NUMA topology: the node count and which nodes have memory
- The caller's cpuset `mems_allowed`
- Kernel version, which decides whether `MPOL_LOCAL`, `MPOL_PREFERRED_MANY`, `MPOL_WEIGHTED_INTERLEAVE` and `MPOL_F_NUMA_BALANCING` exist
- Privilege (`CAP_SYS_NICE`) for `MPOL_MF_MOVE_ALL`
- Where existing pages happen to be placed, which decides `EIO` under `MPOL_MF_STRICT`

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `refuse`. Hermit now presents a kernel built without NUMA instead of a single virtual NUMA node. The six memory-policy syscalls are gated by one predicate, `is_memory_policy_syscall` (`detcore/src/syscall_classification.rs:1311`), refused at one dispatch gate by syscall number before the typed match (`detcore/src/lib.rs:7525`), and included in the deterministic-refusal aggregate, so each returns `ENOSYS` before any argument is examined — Linux compiles `mm/mempolicy.o` only under `CONFIG_NUMA`, and its `cond_syscall` entry points are the same stubs. The single-virtual-node model analysed below is superseded: it fabricated node answers a real no-NUMA kernel never gives, and it made `/proc` NUMA views host-shaped work to maintain.

Target policy: `model` (owner decision 7, 2026-10-04); the p3 plan below was `emulate`, and the Target paragraph at the end of this section replaces it. Hermit's contract is a machine with one virtual NUMA node, stated in the dispatch comment at `detcore/src/lib.rs:2637` and in the classification comment at `detcore/src/syscall_classification.rs:318`. On such a machine the policy can never change where memory goes, so Detcore can synthesize the answer without the kernel. The code already does this: `Syscall::Mbind(_) => Ok(0)` at `detcore/src/lib.rs:2647`. `mbind` is in the Determinized list (`detcore/src/syscall_classification.rs:328`), and the unit test at `detcore/src/syscall_classification.rs:2121` asserts that classification.

Why the p3 plan chose `emulate` and not the alternatives:

- `passthrough`, the legacy recommendation, returns host-dependent results: a node set naming node 1 succeeds on a two-node host and fails with `EINVAL` on a one-node host, and newer modes fail on older kernels.
- `model`, in the p3 plan's sense, meant keeping the kernel as the policy store, whose answer is exactly the host-dependent part. Decision 7's `model` keeps the store in Detcore instead (Target paragraph below).
- `refuse` would break allocators and runtimes, such as jemalloc, libnuma users and JVMs, that call `mbind` and treat failure as fatal or fall back noisily.

What remains before the code fully realizes `emulate` on one virtual node. Every gap below is a case where Linux on a real one-node machine returns an error and Hermit returns 0:

- No argument checking. Invalid modes (F7), forbidden mode-flag combinations (F6), unknown `flags` bits (F10), a non-page-aligned `addr` (F11), a node set with nodes other than 0 only (F8), a non-empty node set with `MPOL_DEFAULT` or `MPOL_LOCAL` (F1, F5), and an empty node set with `MPOL_BIND` or `MPOL_INTERLEAVE` (F2, F3) all return 0. An unreadable `nodemask` is never read, so `EFAULT` (F8) cannot happen.
- No range checking. A range with an unmapped hole returns 0 instead of `EFAULT` (F11).
- No privilege check. `MPOL_MF_MOVE_ALL` returns 0 for any caller (F10). Owner decision 4 settles the answer: the guest holds full capabilities at launch, including `CAP_SYS_NICE`, so 0 is right.
- No policy memory. `get_mempolicy` with `MPOL_F_ADDR` always reports `MPOL_DEFAULT` (`detcore/src/syscalls/misc.rs:734`), even for a range just bound with `MPOL_BIND`. On Linux it reports the bound mode.

On one node, `MPOL_MF_STRICT` and the move flags (F9, F10) never produce `EIO`, so returning 0 for them is already correct when the other arguments are valid.

The dispatch carries `TODO-HUMAN-REVIEW(#720)`, which names https://github.com/rrnewton/hermit/pull/720, the batch-3 change that added these arms.

Target (owner decision 7, with decisions 6 and 4): Detcore records policies per address range in its address-space model, `MemoryMetadata` (`detcore/src/memory.rs:54`). Threads created with `CLONE_VM` share that model, a fork copies it (`detcore/src/lib.rs:1525`), and `execve` replaces it with an empty one (`detcore/src/syscalls/threads.rs:1496`), as Linux drops VMA policies at exec. A successful `mbind` stores the mode, mode flags and node set for the page range, splitting earlier records at the range's ends; `MPOL_DEFAULT` removes the records in the range (F1); `munmap` (`detcore/src/syscalls/files.rs:2946`) drops them and `mremap` (`detcore/src/syscalls/files.rs:2959`) moves them with the pages. `get_mempolicy` with `MPOL_F_ADDR` reports the record (T6, T8). Nothing reaches the host's policy store and placement never changes. The checks follow the kernel's order (mbind(2); kernel `kernel_mbind()` then `do_mbind()` in `mm/mempolicy.c`), which is more precise than the rough order under Linux semantics: (1) `sanitize_mpol_flags()`: a mode at or above `MPOL_MAX`, `MPOL_F_STATIC_NODES` with `MPOL_F_RELATIVE_NODES`, or `MPOL_F_NUMA_BALANCING` with a mode other than `MPOL_BIND` or `MPOL_PREFERRED_MANY` gives `EINVAL` (F6, F7); (2) `get_nodes()`: `maxnode` above `PAGE_SIZE * 8` bits gives `EINVAL` and an unreadable `nodemask` gives `EFAULT` (F8); (3) a `flags` bit outside `MPOL_MF_STRICT | MPOL_MF_MOVE | MPOL_MF_MOVE_ALL` gives `EINVAL` (F10); (4) `MPOL_MF_MOVE_ALL` is accepted, since the guest holds `CAP_SYS_NICE` (owner decision 4) and the privilege rule at `detcore/src/syscalls/threads.rs:2292` never reproduces that `EPERM` (F10); (5) a non-page-aligned `addr` or a wrapping range gives `EINVAL`, and `len == 0` returns 0 before the node set is judged (F11); (6) `mpol_new()` and `mpol_set_nodemask()` on the single virtual node: a non-empty set with `MPOL_DEFAULT` or `MPOL_LOCAL`, an empty set with `MPOL_BIND`, `MPOL_INTERLEAVE`, `MPOL_WEIGHTED_INTERLEAVE` or `MPOL_PREFERRED_MANY`, or a set without node 0 gives `EINVAL` (F1 to F5, F8); (7) a hole in the range gives `EFAULT` (F11). `MPOL_MF_STRICT` and the move flags then return 0 on one node (F9). Step 7 needs the guest's mapping list, which `MemoryMetadata` does not hold (it tracks shared mappings and the break). One way to get it without host dependence is to inject `mbind(addr, len, MPOL_DEFAULT, NULL, 0, 0)` after steps 1 to 6: it fails with `EFAULT` exactly on a hole, and since Hermit never forwards a policy and exec drops inherited VMA policies, it changes no host state. Today none of this exists: `Syscall::Mbind(_) => Ok(0)` (`detcore/src/lib.rs:2647`).

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): map two anonymous pages, then call `mbind` on them with each mode and node set `{0}` (empty for `MPOL_DEFAULT` and `MPOL_LOCAL`, and for `MPOL_PREFERRED` once more). Every call must return 0, the pages must stay readable and writable, and the output must be identical across both runs of `hermit run --verify` on ptrace.
  No existing test at f4055eb9cae4 calls `mbind`, so every T item here needs a new fixture.
- T2 (covers F6, F7): mode 99, `MPOL_F_STATIC_NODES|MPOL_F_RELATIVE_NODES`, and `MPOL_INTERLEAVE|MPOL_F_NUMA_BALANCING` must give `EINVAL`. Today Hermit returns 0.
- T3 (covers F1, F2, F5, F8): node set `{1}` with `MPOL_BIND`, an empty set with `MPOL_BIND`, `{0}` with `MPOL_DEFAULT` and `{0}` with `MPOL_LOCAL` must give `EINVAL`; `maxnode = PAGE_SIZE * 8 + 2` must give `EINVAL`; an unmapped `nodemask` with `MPOL_BIND` must give `EFAULT`.
- T4 (covers F9, F10): `MPOL_BIND {0}` with `MPOL_MF_STRICT`, with `MPOL_MF_MOVE`, and with both must return 0 on one node. `MPOL_MF_MOVE_ALL` must return 0, because the guest holds `CAP_SYS_NICE` (owner decision 4). Flag bit `0x100` must give `EINVAL`.
- T5 (covers F11): `addr + 1` must give `EINVAL`, `len == 0` must return 0, and a range that spans an `munmap`ped page must give `EFAULT`.
- T6 (covers F2, F4): after `mbind(MPOL_BIND, {0})` on a page, `get_mempolicy(&mode, NULL, 0, page, MPOL_F_ADDR)` must report `MPOL_BIND`, as on Linux. Today Hermit reports `MPOL_DEFAULT`.
- T7 (covers F6, F7, F8, F10, F11): check order. Mode 99 with an unmapped `nodemask` gives `EINVAL` (mode before node set); `MPOL_BIND` with an unmapped `nodemask` and flag bit `0x100` gives `EFAULT` (node set copied before `flags` is checked); `MPOL_BIND {1}` with `len == 0` returns 0 (the empty range returns before the node set is judged); `MPOL_BIND {1}` on a range with a hole gives `EINVAL`, not `EFAULT`.
- T8 (covers F1, F2): record lifetime. After `mbind(MPOL_BIND, {0})` on a page, a thread created with `CLONE_VM` and a forked child both read `MPOL_BIND` through `get_mempolicy(MPOL_F_ADDR)`; after `mbind(MPOL_DEFAULT)` on the page, or `munmap` and a new mapping at the same address, it reads `MPOL_DEFAULT`; a program `execve`d afterwards reads `MPOL_DEFAULT` for every address.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/282#issuecomment-5872821948
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/282: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mbind records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/282 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (Detcore returns 0 without the kernel, under the single-virtual-NUMA-node contract); linux_determinism stays conditional; related gains https://github.com/rrnewton/hermit/pull/720 and the sibling issues for set_mempolicy and get_mempolicy; 11 flag behaviours and 6 tests listed; gaps: no argument, range or privilege checks and no remembered policy; no test calls mbind, so tracking_status stays todo; two open questions recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7, 6 and 4 applied (per-range policy record in Detcore's address-space model, read back by get_mempolicy MPOL_F_ADDR, shared under CLONE_VM and dropped at execve; Linux checks in kernel order; MPOL_MF_MOVE_ALL accepted under full launch capabilities); target_policy emulate -> model; new T7 and T8; removed both open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: deterministic refusal added (Hermit presents a kernel built without NUMA; all six memory-policy syscalls return ENOSYS); Summary and Hermit design updated, target_policy -> refuse.

## Discussion

[Start a discussion about `mbind`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mbind&body=Discussion+for+the+%60mbind%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmbind.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mbind%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mbind" (https://github.com/rrnewton/hermit/issues/282), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/282.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mbind`, and by confirming hermit currently routes `mbind` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mbind(0x406000, 4096, MPOL_DEFAULT, NULL, 0, 0) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/282#issuecomment-5872821948
