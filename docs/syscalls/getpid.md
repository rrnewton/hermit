---
schema: syscall-pages/v1
syscall: getpid
nr_x86_64: 39
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1948
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpid&body=Discussion+for+the+%60getpid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 28
---

## Summary

`getpid()` returns the process ID (the thread-group ID) of the caller, as seen in the caller's PID namespace. Detcore traps the call, lets the inner tool perform it exactly once, and then projects a successful positive answer from its shared thread state, so the guest sees Detcore's own process identity on every backend. In a nested PID namespace the delegated namespace-relative answer is kept, and errors pass through unchanged.

## Linux semantics

The call takes no arguments and cannot fail. It returns the thread-group ID of the calling thread translated into the caller's PID namespace. PIDs are allocated per namespace in increasing order from the last one handed out, skipping IDs still in use, and wrap at `pid_max`; threads draw their TIDs from the same sequence. The value is therefore a function of how many tasks the namespace has created before the caller, not of anything the caller passes. With no flags or arguments, the items below list the contexts that decide the value.

### Flags and options

- F1: the return value is the caller's thread-group ID; the call never fails and has no side effects
- F2: every thread of a process gets the same value, which equals `gettid()` only in the thread-group leader
- F3: the value is relative to the caller's PID namespace; under Hermit on ptrace the first guest process is PID 3
- F4: `fork`, `vfork` and `clone` without `CLONE_THREAD` give the child the next free PID in the namespace; `CLONE_THREAD` threads keep the parent's PID but consume a TID from the same sequence
- F5: `execve` keeps the PID; an `execve` from a non-leader thread makes that thread take over the leader's PID, so afterwards `getpid() == gettid()` with the old leader's value
- F6: a request for another PID namespace does not change the caller's own `getpid()`. Under Hermit such a request (`unshare(CLONE_NEWPID)`, `setns` into a PID namespace, `clone`/`clone3` with `CLONE_NEWPID`) gets `EPERM` (owner decision 15), as Linux answers a caller without `CAP_SYS_ADMIN`, so every guest PID stays in the one container namespace and no child is numbered from 1
- F7: allocation skips PIDs still in use and wraps at `pid_max`, so a reaped PID is not reused until the sequence wraps

## Determinism analysis

The value depends only on the PID namespace the caller is in and on the order in which tasks were created in it. On ptrace Hermit starts the guest in a fresh PID namespace (the comment at `detcore/src/syscalls/signal.rs:674` says "Guest PID/TID values are stable in the fresh PID namespace and delivery is scheduler-serialized"), its set-up leaves the first guest process at PID 3 (`detcore/src/consts.rs:29`), and the scheduler serializes thread and process creation, so the same program sees the same PIDs on every run. That holds only with the namespace in place and with creation serialized, which is why `linux_determinism` is conditional.

### Nondeterminism sources

- Running without a fresh PID namespace: the kernel then returns a host PID, which differs on every run. `--no-namespace` runs the guest with no PID namespace (`hermit-cli/src/bin/hermit/run.rs:4536` to `:4542`); that mode is reduced fidelity (see Hermit design).
- Backends that run the guest as a host process: the DBT backend's "native process uses a host PID" (`detcore/src/syscalls/signal.rs:612`). That is outside this ptrace-scoped tracking issue.
- A different number of tasks created before the guest starts inside the namespace, if Hermit's set-up changed; the PID-3 convention at `detcore/src/consts.rs:29` is what pins it.

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `model`. The typed dispatch arm `Syscall::Getpid(_) | Syscall::Gettid(_)` at `detcore/src/lib.rs:7693` delegates the call once, through the normal observation and posthook path, then projects the result with `project_own_getter_result` (`detcore/src/syscalls/namespace.rs:2025`): a positive answer outside a nested PID namespace is replaced by the shared Detcore identity (`own_detpid` for `getpid`, the thread's `dettid` for `gettid`); nonpositive answers and every error keep the delegated value. This supersedes the forwarding design analysed below, which remains as the record of why a forwarded answer was repeatable in the default ptrace namespace but could not be Detcore's identity on backends whose guest runs as a host process.

Target policy: `passthrough`. The code already does this. `getpid` is classified `PassThrough` at `detcore/src/syscall_classification.rs:698`, in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`, whose comment at `detcore/src/syscall_classification.rs:691` gives the condition ("conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions"); it is in the census at `detcore/src/syscall_classification.rs:1707`. The blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. Its virtual cost is the fast class at `detcore/src/syscall_time.rs:37`.

The 2026-08-07 strategy draft (kept under Additional information) asked Hermit to "return the Detcore logical process ID". On ptrace the forwarded value already is that ID:

- Detcore does not number processes itself. It takes the PID the backend reports as its own process identity (`let detpid = DetPid::from_raw(pid.into()); // TODO(T78538674): virtualize pid.` at `detcore/src/lib.rs:1069`), and on ptrace that is the namespace PID.
- Detcore relies on the forwarded value as the virtual PID: when it builds identity snapshots it injects `getpid` into the guest and calls the result `virtual_pid` (`detcore/src/syscalls/files.rs:1496`).
- The unit test `deterministic_refusal_aggregate_includes_fixed_error_families` lists `getpid` among calls that must not be refused, commented `// virtualized` (`detcore/src/syscall_classification.rs:2757`).

So `model` would compute the same number by a second route, and `emulate` would need a PID table that the kernel's `kill`, `wait` and `/proc` would not share. The remaining work is test evidence.

- Reduced fidelity (owner decision 3): under `--no-namespace` (no PID namespace, `hermit-cli/src/bin/hermit/run.rs:4542`; refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on in-process backends, `getpid` returns the host PID, which changes on every run, and Detcore's `detpid` (`detcore/src/lib.rs:1069`) is that host PID too.

Target (owner decision 15, 2026-10-04): the guest cannot create a nested PID namespace, so the PIDs `getpid` returns are always the container namespace's (F6). `unshare` and `setns` already get a fixed `EPERM` (`is_mount_ns_admin_refused_syscall`, `detcore/src/syscall_classification.rs:939`, entries at `detcore/src/syscall_classification.rs:951`-`952`, dispatched at `detcore/src/lib.rs:2216`), but `clone` and `clone3` with `CLONE_NEWPID` are injected with their flags unchanged (`detcore/src/syscalls/threads.rs:924`), and the guest is root in its user namespace, so by this reading the kernel creates the namespace and the child sees PID 1. At the target `handle_clone_family` returns `EPERM` for any `CLONE_NEW*` flag before injecting; https://github.com/rrnewton/hermit/issues/3676 tracks it.

Code facts show `subscribed_record_replay: true`: `getpid` is in the `--passthru-opt` list at `detcore/src/lib.rs:1218`, so it still reaches Detcore in that mode. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module outside the exported tree; this body does not describe how record captures it.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, print raw `syscall(SYS_getpid)` from the main thread and from a second `pthread`; assert the main thread sees 3, both threads see the same value, and the second thread's `gettid()` differs.
  Existing partial coverage: `tests/e2e/determinism-stress/fork_tree.c:36` calls `SYS_getpid` in every process and checks it is positive and equals `gettid()` (cell `determinism-stress-c/fork-tree`, `tests/e2e/manifests/determinism-stress-c.yaml:4`, ptrace verify in CI), but never prints or compares the value; `tests/rust/tkill.rs:50` uses it only to address signals.
- T2 (covers F4, F7): from the root, in a fixed order, `fork`, `vfork`, `clone` (no `CLONE_THREAD`) and `pthread_create`, reaping each child before the next; print every child's `getpid()` and `gettid()` and assert the exact sequence, including that a reaped child's PID is not reused.
  Existing partial coverage: `tests/c/dbt_pid_virtualization.c:166` expects a `vfork` child at PID 8 with parent 3, but its ptrace cell `c-programs/dbt-pid-virtualization` (`tests/e2e/manifests/c-programs.yaml:984`) is disabled in CI with an infrastructure-error reason.
- T3 (covers F5): `execve` a helper that prints its `getpid()` and compare with the value printed before; from a non-leader thread, `execve` and assert `getpid() == gettid() ==` the old leader's PID.
  Existing partial coverage: `hermit-cli/tests/fixtures/nonleader_exec.c:229` checks `my_pid() == leader && my_tid() == leader` after a non-leader exec, driven on ptrace by `run_ptrace_nonleader_exec_preserves_identity_and_time` (`hermit-cli/tests/cli.rs:5520`).
- T4 (covers F6): under `hermit run --verify` on ptrace, `unshare(CLONE_NEWPID)` and a raw `clone` with `CLONE_NEWPID | SIGCHLD` both return `EPERM` (owner decision 15), `getpid()` is unchanged afterwards, and a child then created with `fork` gets the next PID in the container namespace, not 1. Fails at this SHA for the `clone` case, which is forwarded (`detcore/src/syscalls/threads.rs:924`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. getpid is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1948: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional, target_policy investigate -> passthrough (forwarded at detcore/src/lib.rs:2843; on ptrace the namespace PID is Detcore's own process identity, detcore/src/lib.rs:1069, root PID 3 at detcore/src/consts.rs:29); listed 7 context behaviours and 4 tests; open question on host PIDs under --no-namespace; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (host PIDs, and host detpid, under --no-namespace, confirmed to drop the PID namespace at hermit-cli/src/bin/hermit/run.rs:4542, and on in-process backends); target_policy unchanged (passthrough); no new F/T ids; removed the open question on runs without a PID namespace
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 15 applied (no nested PID namespace: unshare/setns already EPERM, clone/clone3 CLONE_NEWPID to get EPERM before injection); target_policy stays passthrough; F6 narrowed to the EPERM target, T4 rewritten to assert it; no new F/T ids; removed the nested-PID-namespace question and its heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: passthrough -> determinized (own-ID getters now project from shared Detcore state); Summary and Hermit design updated, target_policy passthrough -> model.

## Discussion

[Start a discussion about `getpid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpid&body=Discussion+for+the+%60getpid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: getpid" (https://github.com/rrnewton/hermit/issues/1948), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy strategy draft (2026-08-07)

Return the Detcore logical process ID and require every consumer boundary to translate logical and host identities explicitly. Keep the value stable across backend implementation details and record/replay.

Legacy determinism text: getpid returns process identity used by signals, proc paths, tracing, and IPC. Host PIDs vary per run and backend, and related DBI translation defects in issues 1817 and 1818 do not replace classification coverage for getpid itself.

Legacy required tests (2026-08-07):

- Check the initial logical PID and child PIDs after fork, clone, exec, and wait paths.
- Use returned PIDs with kill, tgkill, proc-style paths, and IPC identity checks.
- Assert exact values across strict repeats and record/replay on every backend.
- Plant a host-PID leak at a consumer boundary and prove the identity-binding test refuses it.

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

getpid is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
