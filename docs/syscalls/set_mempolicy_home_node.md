---
schema: syscall-pages/v1
syscall: set_mempolicy_home_node
nr_x86_64: 450
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/439
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_mempolicy_home_node&body=Discussion+for+the+%60set_mempolicy_home_node%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_mempolicy_home_node.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_mempolicy_home_node%60%2C+or+in+what+the+page+says%3F%0A
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

`set_mempolicy_home_node(start, len, home_node, flags)` sets the preferred home NUMA node for address ranges that already carry an `MPOL_BIND` or `MPOL_PREFERRED_MANY` memory policy. Hermit presents a kernel built without NUMA, so Detcore refuses every `set_mempolicy_home_node` with the fixed `ENOSYS` that kernel's stub returns, before it looks at any argument.

## Linux semantics

Added in Linux 5.17. The kernel checks the arguments, then walks the VMAs in `[start, start + len)`. For each VMA with a VMA memory policy of `MPOL_BIND` or `MPOL_PREFERRED_MANY`, it records `home_node` as the node to allocate from first. VMAs without a VMA policy are skipped. The call changes only where future pages are placed, never memory contents.

### Flags and options

- F1: `home_node`: must be below `MAX_NUMNODES` and an online node, otherwise `EINVAL`.
- F2: `flags`: reserved, must be 0, otherwise `EINVAL`.
- F3: `start` and `len`: an unaligned `start` returns `EINVAL`; `len` is rounded up to a page; a range that wraps returns `EINVAL`; `len` of 0 returns 0.
- F4: VMAs with an `MPOL_BIND` or `MPOL_PREFERRED_MANY` policy: their home node is set and the call returns 0.
- F5: A VMA with any other VMA policy (for example `MPOL_PREFERRED` or `MPOL_INTERLEAVE`): `EOPNOTSUPP`, and the walk stops there, so VMAs earlier in the range keep their new home node.
- F6: VMAs with no VMA policy, and unmapped gaps: skipped; if nothing in the range was updated the call returns `ENOENT`, as the 2026-07-24 strace below shows.

## Determinism analysis

`linux_determinism: conditional`. The call has no effect on computed results, but its return value depends on host and process state.

### Nondeterminism sources

- Whether `home_node` is valid (F1) depends on which NUMA nodes the host has online.
- The result for a range (F4, F5, F6) depends on the VMA policies set earlier by `mbind`, which on a real host can depend on topology-dependent choices in the program.
- The placement effect of a successful call depends on host memory availability per node, but is visible only through placement queries and timing.

The call repeats for a fixed host topology and a fixed sequence of earlier `mbind` calls.

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `refuse`. Hermit now presents a kernel built without NUMA instead of a single virtual NUMA node. The six memory-policy syscalls are gated by one predicate, `is_memory_policy_syscall` (`detcore/src/syscall_classification.rs:1311`), refused at one dispatch gate by syscall number before the typed match (`detcore/src/lib.rs:7525`), and included in the deterministic-refusal aggregate, so each returns `ENOSYS` before any argument is examined — Linux compiles `mm/mempolicy.o` only under `CONFIG_NUMA`, and its `cond_syscall` entry points are the same stubs. The single-virtual-node model analysed below is superseded: it fabricated node answers a real no-NUMA kernel never gives, and it made `/proc` NUMA views host-shaped work to maintain.

Target policy: `emulate`. Detcore answers from its own single-node model and never forwards the call, so no host topology reaches the guest.

What the code does at hermit f4055eb9cae4:

- Classification: determinized in the NUMA and scheduling-policy block, whose comment says Hermit "presents a single deterministic virtual CPU and a single virtual NUMA node" and determinizes these calls "to fixed, host-independent results" (`detcore/src/syscall_classification.rs:318-325`, entry at `detcore/src/syscall_classification.rs:331`).
- Dispatch and handler: the pinned Reverie has no typed variant, so a Sysno-guarded arm returns `Ok(0)` for every call (`detcore/src/lib.rs:2198-2202`; comment at `detcore/src/lib.rs:2194-2197`).
- The rest of the model: `mbind` and `set_mempolicy` return 0 without effect (`detcore/src/lib.rs:2647-2648`), and `get_mempolicy` always reports `MPOL_DEFAULT` (`detcore/src/syscalls/misc.rs:731-737`).

Why emulate: forwarding would expose host NUMA topology, and Hermit's `mbind` never sets a kernel policy, so a forwarded call would answer about kernel state the guest believes it changed. A fixed answer from the single-node model is host-independent and repeats.

Known gaps (where today's constant 0 differs from what Linux returns on a one-node machine):

- F1, F2 and F3: `home_node` other than 0, nonzero `flags`, an unaligned `start` and a wrapping range all return 0 instead of `EINVAL`.
- F5 and F6: Hermit's own `get_mempolicy` says every range has the default policy, under which Linux returns `ENOENT` for a non-empty range. But Hermit's `mbind` reports success, and a guest that called `mbind(MPOL_BIND)` first expects 0, or `EOPNOTSUPP` after `mbind(MPOL_PREFERRED)`. Returning the Linux answer in both cases needs Detcore to remember the policy mode each `mbind` set per range. The alternative is to answer as if no policy were ever set, which contradicts the earlier `mbind` success. Owner decision 7 chooses the first: the mbind body now records per-range policies (Target paragraph below).
- No test calls `set_mempolicy_home_node`.
- The recommended tests give the Linux values for a single-node machine; today Hermit returns 0 for all of them.

Target (owner decision 6, with decision 7): the policy stays `emulate`; the home node changes only future placement and no call reports it, so there is nothing of its own to read back. The checks follow the kernel's order (`set_mempolicy_home_node` syscall in `mm/mempolicy.c`): (1) an unaligned `start` gives `EINVAL` (F3); (2) non-zero `flags` gives `EINVAL` (F2); (3) a `home_node` other than 0, the virtual machine's only online node, gives `EINVAL` (F1); (4) a wrapping range gives `EINVAL`, and `len` 0, after rounding up to a page, returns 0 (F3); (5) the walk over the range reads the per-range policy records that the mbind body adds to `MemoryMetadata` (`detcore/src/memory.rs:54`) under decision 7: ranges with no record, and unmapped gaps, are skipped; a record of `MPOL_BIND` or `MPOL_PREFERRED_MANY` is updated and makes the result 0 (F4); a record of any other mode stops the walk with `EOPNOTSUPP`, keeping updates already made (F5); if nothing was updated the result is `ENOENT` (F6). Today the Sysno-guarded arm returns 0 for every call (`detcore/src/lib.rs:2198-2202`).

## Recommended tests

- T1 (covers F1, F2): On a fresh anonymous page with `mbind(MPOL_BIND, node 0)` applied, `home_node` of 1 and of 1024, and `flags` of 1, each return `EINVAL`; the result is identical in both `--strict --verify` runs.
- T2 (covers F3): `start` not page-aligned returns `EINVAL`; `len` of 0 returns 0; a range from the top of the address space that wraps returns `EINVAL`.
- T3 (covers F4): After `mbind(MPOL_BIND, node 0)` on a mapping, `set_mempolicy_home_node(addr, len, 0, 0)` returns 0 and the mapping's contents are unchanged.
- T4 (covers F5, F6): A fresh mapping with no `mbind` returns `ENOENT`; a mapping given `MPOL_PREFERRED` by `mbind` returns `EOPNOTSUPP`.
- T5 (covers F1, F2, F3, F5, F6): check order and walk. An unaligned `start` with `flags` 1 gives `EINVAL`; `home_node` 1 with `len` 0 gives `EINVAL` (the node is checked before the empty range returns); `len` 0 with valid arguments on a range with no policy returns 0, not `ENOENT`; a two-page range whose first page has `MPOL_BIND` and second `MPOL_INTERLEAVE` gives `EOPNOTSUPP`; the same range after `munmap` of both pages gives `ENOENT`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/439: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch sysno-guard.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (single virtual NUMA node; the arm at detcore/src/lib.rs:2198-2202 returns 0 for every call, which skips Linux's EINVAL, ENOENT and EOPNOTSUPP results); linux_determinism stays conditional; 6 flag behaviours and 4 tests listed; no test calls set_mempolicy_home_node, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 7 applied (Linux checks in kernel order; the range walk reads the per-range mbind policy records that decision 7 adds, so 0, EOPNOTSUPP and ENOENT follow the guest's earlier mbind calls); target_policy stays emulate (the home node itself is never reported); new T5

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: deterministic refusal added (Hermit presents a kernel built without NUMA; all six memory-policy syscalls return ENOSYS); Summary and Hermit design updated, target_policy -> refuse.

## Discussion

[Start a discussion about `set_mempolicy_home_node`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_mempolicy_home_node&body=Discussion+for+the+%60set_mempolicy_home_node%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_mempolicy_home_node.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_mempolicy_home_node%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: set_mempolicy_home_node" (https://github.com/rrnewton/hermit/issues/439), legacy variant A1.

The evidence below says Hermit routed the call through an unclassified temporary pass-through block. That was true on 2026-07-24 and is stale at f4055eb9cae4; Hermit design above describes the current arm.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=set_mempolicy_home_node` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `set_mempolicy_home_node` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
set_mempolicy_home_node(0x407000, 4096, 0, 0) = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
