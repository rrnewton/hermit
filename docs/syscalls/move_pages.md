---
schema: syscall-pages/v1
syscall: move_pages
nr_x86_64: 279
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/297
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+move_pages&body=Discussion+for+the+%60move_pages%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmove_pages.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60move_pages%60%2C+or+in+what+the+page+says%3F%0A
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

`move_pages` moves individual pages of a process to specified NUMA nodes, and/or queries the node each page currently resides on. Hermit presents a kernel built without NUMA, so Detcore refuses every `move_pages` with the fixed `ENOSYS` that kernel's stub returns, before it looks at any argument.

Manual reference: Linux man-pages move_pages(2), numa(7).

## Linux semantics

`move_pages(pid, count, pages, nodes, status, flags)` operates on `count` page addresses in the memory of process `pid`.

- **Query mode** (`nodes == NULL`): the kernel writes the current node of each page into `status[i]`. A page that is not present gets `-ENOENT`, and an address with no mapping gets `-EFAULT`.
- **Move mode** (`nodes != NULL`): the kernel tries to migrate page `i` to `nodes[i]` and writes the resulting node, or a negative errno, into `status[i]`.

The return value is 0 on full success, a positive count of pages that could not be moved for non-fatal reasons, or -1 with `errno` for a call-level error. status[] array is filled with node numbers or negative errnos. Verified query mode returns 0 with node ids on this host.

### Flags and options

- F1: `pid`: 0 means the calling process. Another pid needs `PTRACE_MODE_READ_REALCREDS` access to that process (`EPERM` otherwise), and a pid that does not exist gives `ESRCH`.
- F2: query mode (`nodes == NULL`): `status[i]` receives the node holding page `i`, `-ENOENT` for a page that is not present (never touched, or a shared zero page), or `-EFAULT` for an unmapped address.
- F3: move mode (`nodes != NULL`): each page is migrated to `nodes[i]`. The per-page result in `status[i]` is the new node or a negative errno (`-EACCES` for a page mapped by several processes without `MPOL_MF_MOVE_ALL`, `-EBUSY`, `-EFAULT`, `-ENOENT`, `-ENOMEM`, `-EIO`, `-EINVAL`). A target node that is not online gives `ENODEV` for the call. A node the task's cpuset does not allow gives `EACCES`.
- F4: `MPOL_MF_MOVE` (the only bit for an ordinary process): move only pages mapped by this process alone.
- F5: `MPOL_MF_MOVE_ALL`: also move shared pages. It needs `CAP_SYS_NICE` (`EPERM` otherwise). Any flag bit other than these two gives `EINVAL`.
- F6: array arguments: an unreadable `pages` or `nodes` array, or an unwritable `status` array, gives `EFAULT`. `count = 0` returns 0 without touching the arrays.
- F7: on a kernel built without `CONFIG_NUMA` the syscall returns `ENOSYS`.

## Determinism analysis

In query mode it returns the NUMA node id of each page, which reflects host placement/topology and varies across hosts and runs. It does not affect computed results.

Move mode outcomes also depend on host state: which nodes are online, cpuset membership, whether a page is busy or shared at that instant, and memory pressure on the target node. In both modes, whether a page is present (`-ENOENT`) depends on reclaim and swap.

### Nondeterminism sources

Returned per-page node ids reflect host NUMA state (nondeterministic). No effect on results.

- Page presence (`-ENOENT` versus a node id) depends on whether the page was touched and whether the host reclaimed or swapped it.
- Move-mode per-page errnos (`-EBUSY`, `-ENOMEM`) depend on host contention and free memory.
- The set of online nodes and the cpuset depend on the host.

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `refuse`. Hermit now presents a kernel built without NUMA instead of a single virtual NUMA node. The six memory-policy syscalls are gated by one predicate, `is_memory_policy_syscall` (`detcore/src/syscall_classification.rs:1311`), refused at one dispatch gate by syscall number before the typed match (`detcore/src/lib.rs:7525`), and included in the deterministic-refusal aggregate, so each returns `ENOSYS` before any argument is examined — Linux compiles `mm/mempolicy.o` only under `CONFIG_NUMA`, and its `cond_syscall` entry points are the same stubs. The single-virtual-node model analysed below is superseded: it fabricated node answers a real no-NUMA kernel never gives, and it made `/proc` NUMA views host-shaped work to maintain.

Target policy: `emulate`. The guest sees one virtual NUMA node, and `get_mempolicy` reports the default single-node policy (`detcore/src/syscalls/misc.rs:719-738`). `move_pages` should answer from that model and never reach the kernel, because a real migration would let host NUMA state into the guest and gains nothing on one virtual node.

The dispatch arm is `detcore/src/lib.rs:2651`, inside the NUMA batch block (`detcore/src/lib.rs:2638-2650`, tag `TODO-HUMAN-REVIEW(#720)`, which refers to https://github.com/rrnewton/hermit/pull/720). `handle_move_pages` (`detcore/src/syscalls/misc.rs:740-759`) ignores `pid`, `pages`, `nodes` and `flags`. If `status` is non-NULL it writes `count` zeros there, then it returns 0. The answer is a constant, so it is identical across `--verify` runs and record/replay.

Why `emulate` and not the alternatives:

- `passthrough` or `model` would report host node ids and host page presence.
- `refuse` would break NUMA-aware allocators and runtimes that query placement.
- On one node a constant answer is a faithful model of an idle migration.

What remains before the code fully realizes `emulate` (gaps measured by reading the handler; none is exercised by a test):

- F1: no pid check. A nonexistent pid returns 0 where Linux gives `ESRCH`.
- F3: a target node other than 0 is accepted and reported as node 0, where Linux gives `ENODEV` for a node that does not exist.
- F5: no flag validation. Unknown bits succeed, where Linux gives `EINVAL`. `MPOL_MF_MOVE_ALL` also succeeds, which is the right answer: the guest holds full capabilities at launch, including `CAP_SYS_NICE` (owner decision 4).
- F6: `pages` and `nodes` are never read, so an unreadable array does not give `EFAULT`. A NULL `status` with `count > 0` returns 0, where Linux faults.
- F2: pages that are not present, and addresses with no mapping, are reported as node 0 rather than `-ENOENT` or `-EFAULT`. The first is deliberate (see Open questions). The second is not.
- Hypothesis: `vec![0i32; count]` (`detcore/src/syscalls/misc.rs:754-756`) allocates `4 * count` bytes in the tracer for any guest-supplied `count`. A huge `count` with a non-NULL `status` could make the tracer abort on allocation failure instead of returning an error. Not measured.

Target (owner decision 6, with decision 4): the policy stays `emulate`; on one node every page already sits on node 0, so there is nothing to read back beyond that. Decision 6 owes Linux's checks in the kernel's order (move_pages(2); kernel `kernel_move_pages()` in `mm/migrate.c`): (1) a `flags` bit other than `MPOL_MF_MOVE` and `MPOL_MF_MOVE_ALL` gives `EINVAL` (F5); (2) `MPOL_MF_MOVE_ALL` is accepted, because the guest holds `CAP_SYS_NICE` (owner decision 4) and the privilege rule at `detcore/src/syscalls/threads.rs:2292` never reproduces that `EPERM` (F5); (3) the target, as `find_mm_struct()` resolves it: `pid` 0 is the caller, any other pid is looked up with `thread_is_live` (`detcore/src/tool_global.rs:4032`), no match gives `ESRCH`, and the ptrace-access `EPERM` is never owed under full capabilities (F1); (4) in move mode, `do_pages_move()` reads `pages[i]` and `nodes[i]` in turn (`EFAULT` if unreadable), and a node other than 0 ends the call with `ENODEV`, the virtual machine having no other node (F3, F6); each page then gets status 0, or `-EFAULT` for an address with no mapping, written as it goes (`EFAULT` for the call if `status` is unwritable) (F3, F4, F6); (5) in query mode, `do_pages_stat()` reads `pages` and writes `status` in chunks of 16 (`DO_PAGES_STAT_CHUNK_NR`), so an unreadable `pages` or unwritable `status` gives `EFAULT`, and each address gets node 0, or `-EFAULT` with no mapping (F2, F6); `count = 0` returns 0 before either array is touched. Working in fixed chunks also removes the guest-sized `vec![0i32; count]` allocation in the tracer (`detcore/src/syscalls/misc.rs:754`). Pages that are present and pages that are not both report node 0 until the open question below is answered. Detecting an address with no mapping needs the guest's mapping list, the same need as `mbind`'s hole check. Today the handler ignores every argument but `status` (`detcore/src/syscalls/misc.rs:746`).

## Recommended tests

- T1 (covers F2, F6): a raw `SYS_move_pages` query with `pid = 0` on two pages, one touched and one not present, asserts status 0 for both over four iterations under `--verify`. Existing candidate: `tests/c/numa_node_identity.c:44` (query helper) and its loop at `tests/c/numa_node_identity.c:126-130`. The cell is `c-programs/numa-node-identity` (`tests/e2e/manifests/c-programs.yaml:12704`), verify ci on ptrace, kvm and liteinst, with dbt disabled. Extend it with `count = 0` returning 0.
- T2 (covers F1): `pid = getpid()` succeeds like `pid = 0`. A pid that does not exist gives `ESRCH`. No existing test.
- T3 (covers F3, F4): move mode with `nodes = {0, 0}` and `MPOL_MF_MOVE` returns 0 with status 0. `nodes = {1}` gives `ENODEV` on the single virtual node. No existing test.
- T4 (covers F5): `MPOL_MF_MOVE_ALL` returns 0 with status 0, because the guest holds `CAP_SYS_NICE` (owner decision 4). An unknown flag bit gives `EINVAL`. No existing test.
- T5 (covers F6): an unreadable `pages`, an unreadable `nodes` in move mode, and an unwritable `status` each give `EFAULT`. An unmapped page address in an otherwise valid array is reported per page. No existing test.
- T6 (covers F2, F3): record and replay of a query and a move call reproduce the return value and `status[]`. No existing test.
- T7 (covers F7): no Hermit test is possible on a NUMA-enabled host kernel. Record that the emulated answer never returns `ENOSYS`, which matches a kernel built with `CONFIG_NUMA`. Document-only.
- T8 (covers F1, F3, F5, F6): check order. Flag bit `0x100` with a nonexistent pid gives `EINVAL` (flags first); `MPOL_MF_MOVE_ALL` with a nonexistent pid gives `ESRCH`; a nonexistent pid with an unreadable `pages` array gives `ESRCH` (target before the arrays); in move mode, `nodes = {1}` with an unwritable `status` gives `ENODEV` (the node is checked before that page's status is written); a `count` of `1 << 30` with an unmapped `pages` array gives `EFAULT` and the tracer stays up.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/297#issuecomment-5872855312
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/297: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; move_pages records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/297 reopens.

- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (single virtual NUMA node; handler writes node 0 into every status slot and returns 0 without entering the kernel); linux_determinism stays nondeterministic; 7 flag behaviours and 7 tests listed; argument-validation gaps recorded; open question for the owner on reporting -ENOENT for pages that are not present; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 4 applied (Linux checks in kernel order: flags, target against Detcore's thread table, then the arrays page by page; ENODEV for a node other than 0; -EFAULT status for an unmapped address; fixed-size chunks instead of a guest-sized tracer allocation; MPOL_MF_MOVE_ALL accepted under full launch capabilities); target_policy stays emulate; new T8; T4 now expects 0 for MPOL_MF_MOVE_ALL; the -ENOENT open question is kept, since no decision answers it
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/numa_node_identity.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: deterministic refusal added (Hermit presents a kernel built without NUMA; all six memory-policy syscalls return ENOSYS); Summary and Hermit design updated, target_policy -> refuse.

## Discussion

[Start a discussion about `move_pages`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+move_pages&body=Discussion+for+the+%60move_pages%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmove_pages.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60move_pages%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: move_pages" (https://github.com/rrnewton/hermit/issues/297), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/297.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=move_pages`, and by confirming hermit currently routes `move_pages` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
move_pages(0, 1, [0x406000], NULL, [0], 0) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/297#issuecomment-5872855312

The "UNCLASSIFIED temporary-passthrough block" described in the 2026-07-24 evidence above is historical. At f4055eb9cae4 `move_pages` is dispatched to `handle_move_pages` (`detcore/src/lib.rs:2651`).

### Open questions

- Should a page that is not present be reported as `-ENOENT`, as Linux does, rather than node 0? `tests/c/numa_node_identity.c:14-28` pins node 0 deliberately, because presence depends on host reclaim and swap. Reporting `-ENOENT` faithfully would need Detcore to track which guest pages were touched, and even then a swapped-out page reads as present on Linux. This is a fidelity-versus-simplicity policy decision for the owner.
