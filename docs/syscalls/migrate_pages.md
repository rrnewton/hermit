---
schema: syscall-pages/v1
syscall: migrate_pages
nr_x86_64: 256
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/284
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+migrate_pages&body=Discussion+for+the+%60migrate_pages%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmigrate_pages.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60migrate_pages%60%2C+or+in+what+the+page+says%3F%0A
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

`migrate_pages(pid, maxnode, old_nodes, new_nodes)` moves every page of a process that sits on one set of NUMA nodes to another set, and returns the number of pages it could not move. Hermit presents a kernel built without NUMA, so Detcore refuses every `migrate_pages` with the fixed `ENOSYS` that kernel's stub returns, before it looks at any argument.

Manual reference: Linux man-pages migrate_pages(2).

## Linux semantics

The kernel reads `old_nodes` and `new_nodes` as bitmaps of `maxnode` bits, finds the target process (`pid` 0 means the caller), checks that the caller may act on it, and walks its address space. Pages on a node in `old_nodes` move to the corresponding node in `new_nodes`, keeping their relative position in the set where possible. Without `CAP_SYS_NICE` only pages mapped by this process alone move; with it, shared pages move too. The return value is the number of pages that could not be moved (pinned, locked, under writeback or otherwise busy), or an error.

The new node set is intersected with the caller's cpuset `mems_allowed`. A new set that is not a subset of the target's allowed nodes needs `CAP_SYS_NICE`, otherwise `EPERM`. If the intersection is empty the call fails with `EINVAL`. On a single-node machine a valid call is inert: every page is already on node 0, and only node 0 is allowed.

Measured on the host (Linux 7.1.3, NUMA nodes 0 and 1, a Python `ctypes` harness): `migrate_pages(0, 0, NULL, NULL)` gave `EINVAL`; `migrate_pages(0, 65, {0}, {0})` and `migrate_pages(0, 65, {0}, {1})` returned 0; `maxnode = 1 << 40` gave `EINVAL`; `pid = 2^30` gave `ESRCH`; an unmapped `old_nodes` pointer gave `EFAULT`.

### Flags and options

- F1: `pid`: 0 or the caller's own pid acts on the caller. Another pid acts on that process, which needs ptrace-read access to it (otherwise `EPERM`). A pid that does not exist gives `ESRCH`.
- F2: `maxnode`: the number of bits to read from each mask. 0 (or a NULL mask) means an empty set. A value larger than the page-sized bitmap limit gives `EINVAL`, and set bits beyond the kernel's node limit give `EINVAL`.
- F3: `old_nodes`: the nodes to move pages from. An empty set moves nothing.
- F4: `new_nodes`: the nodes to move pages to. After intersection with the caller's cpuset an empty set gives `EINVAL` (host probe: empty masks gave `EINVAL`). A node outside the target's cpuset gives `EPERM` without `CAP_SYS_NICE`.
- F5: privilege: `CAP_SYS_NICE` lets shared pages move and lets `new_nodes` name nodes outside the target's cpuset.
- F6: return value: the count of pages that could not be moved, 0 when every page moved or nothing needed to move.
- F7: errors: `EFAULT` (an unreadable mask), `EINVAL` (bad `maxnode`, empty target set, or a target without an address space), `EPERM`, `ESRCH`, `ENOMEM`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. NUMA placement never changes the bytes a program computes, but the return value does not repeat even on one host with the same arguments: the count of pages that could not move (F6) depends on transient page state, such as pages pinned for I/O, locked, or under writeback at the moment of the call. Across hosts, the node count and the cpuset decide whether a node set is accepted (F4), and privilege decides `EPERM` (F1, F5). The legacy issue https://github.com/rrnewton/hermit/issues/284 recorded the same conclusion.

### Nondeterminism sources

- Transient page state at call time, which decides the count of pages not moved
- Host NUMA topology: the node count and which nodes have memory
- The caller's and the target's cpuset `mems_allowed`
- Privilege (`CAP_SYS_NICE`) and ptrace access to another process
- Which processes exist, for a pid other than the caller's

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `refuse`. Hermit now presents a kernel built without NUMA instead of a single virtual NUMA node. The six memory-policy syscalls are gated by one predicate, `is_memory_policy_syscall` (`detcore/src/syscall_classification.rs:1311`), refused at one dispatch gate by syscall number before the typed match (`detcore/src/lib.rs:7525`), and included in the deterministic-refusal aggregate, so each returns `ENOSYS` before any argument is examined — Linux compiles `mm/mempolicy.o` only under `CONFIG_NUMA`, and its `cond_syscall` entry points are the same stubs. The single-virtual-node model analysed below is superseded: it fabricated node answers a real no-NUMA kernel never gives, and it made `/proc` NUMA views host-shaped work to maintain.

Target policy: `emulate`. Hermit's contract is a machine with one virtual NUMA node, stated in the dispatch comment at `detcore/src/lib.rs:2638-2644` and in the classification comment at `detcore/src/syscall_classification.rs:318-325`. On such a machine there is nowhere to move a page, so Detcore can synthesize the answer without the kernel. The code already does this: `Syscall::MigratePages(_) => Ok(0)` at `detcore/src/lib.rs:2650`. `migrate_pages` is in the Determinized list (`detcore/src/syscall_classification.rs:332`), and the unit test `representative_policies_stay_in_their_reviewed_sections` asserts that classification (`detcore/src/syscall_classification.rs:2129`, in the batch-3 loop that starts at `detcore/src/syscall_classification.rs:2124`). This matches the sibling NUMA calls: `mbind` and `set_mempolicy` also return 0 (`detcore/src/lib.rs:2647-2648`), and `get_mempolicy` and `move_pages` report node 0 (`detcore/src/syscalls/misc.rs:719-759`).

Why `emulate` and not the alternatives:

- `passthrough`, which the legacy "NEEDS-INVESTIGATION" left open, returns host-dependent results: on a two-node host, `{0} -> {1}` really moves pages and the count of failures varies; on a one-node host it fails.
- `model` would keep the kernel in the loop, but the kernel's answer is exactly the host-dependent part.
- `refuse` would break tools such as `migratepages` and libnuma users that treat failure as fatal, for no gain: on one node the call has no effect to protect.

What remains before the code fully realizes `emulate` on one virtual node. Every gap below is a case where Linux on a real one-node machine returns an error and Hermit returns 0:

- No argument checking. An empty `new_nodes` or one naming only nodes other than 0 (F4), an oversized `maxnode` (F2), and an unreadable mask (F7) all return 0. The masks are never read.
- No pid checking. A pid that does not exist returns 0 instead of `ESRCH`, and a pid outside the container is accepted (F1). The virtual pid is not translated, because it is never used.
- No privilege check (F5). Owner decision 4 settles what the guest sees: it holds full capabilities at launch, including `CAP_SYS_NICE` and ptrace access to its own processes, so no `EPERM` is owed.
- The call is not in the `--passthru-opt` allow-list in `Detcore::subscriptions` (`detcore/src/lib.rs:1100-1104`; option at `detcore-model/src/config.rs:312-317`), so under that explicit performance opt-in it bypasses Detcore and really migrates host pages.

For valid arguments on one node, 0 (F3, F6) is the Linux answer, so the emulation is already correct there.

The dispatch carries `TODO-HUMAN-REVIEW(#720)`, which names https://github.com/rrnewton/hermit/pull/720, the batch-3 change that added these arms.

Target (owner decision 6, with decisions 4 and 7): the policy stays `emulate`, since there is no state to read back: on one node no page ever moves, so the answer is 0 or an error. Decision 6 owes Linux's checks in the kernel's order (migrate_pages(2); kernel `kernel_migrate_pages()` in `mm/mempolicy.c`), answered from the guest's arguments and Detcore's own tables: (1) `get_nodes()` on `old_nodes`, then on `new_nodes`: `maxnode` above the page-sized bitmap limit or set bits beyond the node limit give `EINVAL`, an unreadable mask gives `EFAULT` (F2, F7); (2) the target: `pid` 0 is the caller, any other pid is looked up with `thread_is_live` (`detcore/src/tool_global.rs:4032`, used the same way at `detcore/src/syscalls/threads.rs:2423`), and no match gives `ESRCH` (F1); (3) the ptrace-access and `CAP_SYS_NICE` checks pass, since the guest holds full capabilities (owner decision 4) and the privilege rule at `detcore/src/syscalls/threads.rs:2292` never reproduces their `EPERM` (F1, F5); (4) `new_nodes` intersected with the virtual machine's only node: an empty result, which includes `maxnode = 0`, a NULL `new_nodes` and `{1}`, gives `EINVAL` (F4); (5) otherwise 0 pages could not move, so the call returns 0 (F3, F6). Today none of this exists: `Syscall::MigratePages(_) => Ok(0)` (`detcore/src/lib.rs:2650`).

## Recommended tests

- T1 (covers F1, F3, F6): map and touch several pages, then call `migrate_pages(0, 65, {0}, {0})`, `migrate_pages(getpid(), 65, {0}, {0})` and the same with an empty `old_nodes`; each must return 0, the pages must keep their contents, and output must be identical across both runs of `hermit run --verify` on ptrace.
- T2 (covers F2, F4): `maxnode = 0`, `maxnode = 1 << 40`, `new_nodes = {}` and `new_nodes = {1}` must give `EINVAL`, as on a one-node Linux machine for a caller with `CAP_SYS_NICE` (owner decision 4). Today Hermit returns 0.
- T3 (covers F1, F5, F7): a nonexistent pid must give `ESRCH`, an unmapped `old_nodes` pointer `EFAULT`, and a call on a child process with `{0} -> {0}` must return 0, since the guest holds full capabilities (owner decision 4). Today Hermit returns 0 for all three.
- T4 (covers F1, F6): fork a child, call `migrate_pages(child_pid, 65, {0}, {0})` from the parent, and check both processes still run and print identical output across verify runs.
- T5 (covers F1, F2, F4, F7): check order. An unmapped `old_nodes` with a nonexistent pid gives `EFAULT` (masks before the target); a valid `{0}` mask pair with a nonexistent pid gives `ESRCH`; `new_nodes = {1}` with a nonexistent pid gives `ESRCH`, not `EINVAL` (target before the node-set check); `new_nodes = {1}` with `pid` 0 gives `EINVAL`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/284#issuecomment-5872826552
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/284: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; migrate_pages records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/284 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (Detcore returns 0 without the kernel, under the single-virtual-NUMA-node contract, like mbind and set_mempolicy); linux_determinism stays nondeterministic (the return value counts pages that failed to move); 7 flag behaviours and 4 tests listed; gaps: no argument, pid, privilege or pointer checks, and the call reaches the host under --passthru-opt; no test calls migrate_pages, so tracking_status stays todo; open questions recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 4 applied (Linux checks in kernel order: masks, then the target against Detcore's thread table, then the node set on one virtual node; no EPERM under full launch capabilities); target_policy stays emulate (nothing to read back on one node); new T5; removed both open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: deterministic refusal added (Hermit presents a kernel built without NUMA; all six memory-policy syscalls return ENOSYS); Summary and Hermit design updated, target_policy -> refuse.

## Discussion

[Start a discussion about `migrate_pages`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+migrate_pages&body=Discussion+for+the+%60migrate_pages%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmigrate_pages.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60migrate_pages%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: migrate_pages" (https://github.com/rrnewton/hermit/issues/284), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/284.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=migrate_pages`, and by confirming hermit currently routes `migrate_pages` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
migrate_pages(0, 0, NULL, NULL) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/284#issuecomment-5872826552
