---
name: cpuset-alloc
description: Reserve a disjoint host CPU set for one process, inspect live reservations, or reclaim stale owner records. Use when concurrent jobs need non-overlapping CPU affinity outside a larger DAG.
---

# cpuset-alloc

Use the allocator when independent processes need a shared, lock-protected CPU reservation ledger.
Normal process exit releases the reservation; stale records from abrupt death are reclaimed by PID
and process start time on the next allocator operation. The default ledger is
`$XDG_RUNTIME_DIR/dagrun/core-reservations.json`, falling back under `${TMPDIR:-/tmp}/dagrun-$UID/`;
`DAGRUN_CORE_LEDGER` overrides it. Because `status` also sweeps stale records, inspect the JSON
directly first when preserving crash evidence matters.

The installed command is the source of truth:

- `cpuset-alloc --help`
- `cpuset-alloc status --help`
- `cpuset-alloc reclaim --help`

The allocator is also integrated into `dagrun` for DAG steps.
