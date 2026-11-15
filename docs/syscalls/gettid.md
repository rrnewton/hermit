---
schema: syscall-pages/v1
syscall: gettid
nr_x86_64: 186
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1949
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+gettid&body=Discussion+for+the+%60gettid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgettid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60gettid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 101
---

## Summary

`gettid()` returns the calling thread's id. Detcore traps the call, lets the inner tool perform it exactly once, and then projects a successful positive answer from its shared thread state, so the guest sees Detcore's own thread identity on every backend. In a nested PID namespace the delegated answer is kept, and errors pass through unchanged.

## Linux semantics

`gettid` takes no arguments and cannot fail. It returns the id of the calling task as seen from the caller's PID namespace. The same id appears in many other places: `CLONE_PARENT_SETTID` and `CLONE_CHILD_SETTID` writes, `set_tid_address`, futex owner words for priority-inheritance and robust futexes, `tgkill` and `tkill` targets, `sched_*` and `ioprio_*` arguments, and `/proc/<pid>/task/<tid>`. A `gettid` result must agree with all of them. The kernel allocates the next free id above the last one handed out in that namespace, wraps at `pid_max`, and reuses the ids of reaped tasks.

### Flags and options

- F1: main thread: returns the thread-group id, equal to `getpid()`
- F2: thread created with `CLONE_THREAD` (as by `pthread_create`): returns an id unique among live tasks in the namespace and different from `getpid()`. It equals the value the kernel wrote through `CLONE_PARENT_SETTID`, which glibc stores in the thread descriptor.
- F3: new process (`fork`, `vfork`, or `clone` without `CLONE_THREAD`): the child's `gettid()` equals its `getpid()`, which is the value the parent got back from `fork`
- F4: `execve` from a non-leader thread: the kernel kills the other threads, and the calling thread takes over the leader's id. After the exec, `gettid() == getpid() ==` the old leader's id.
- F5: PID namespace: the value is relative to the caller's PID namespace. A process in a new namespace sees small ids. Hermit's convention is that the guest's first process is id 3 (`detcore/src/consts.rs:29`-`30`).
- F6: allocation order and reuse: ids are allocated in order of task creation from the namespace's last allocated id, wrap at `pid_max` and reuse ids of reaped tasks. A task with `CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE` over the namespace can steer allocation through `/proc/sys/kernel/ns_last_pid`. `clone3`'s `set_tid`, which steers it too on Linux, gets `EPERM` under Hermit and allocates no id (owner decision 15).

## Determinism analysis

On Linux the value is conditional. In a namespace shared with other workloads, it depends on how many tasks the rest of the system created first, so it differs between runs. In a fresh PID namespace, F5 and F6 make it a function of the order in which tasks were created and reaped in that namespace. Under Hermit that order is fixed when three conditions hold:

1. The guest runs in a fresh PID namespace for each run, so allocation starts from the same point (`detcore/src/consts.rs:29`-`30`; the test harness does this at `detcore/tests/testutils/src/lib.rs:666`).
2. Hermit's scheduler orders the guest's `clone`, `fork`, exit and reap events identically in every run.
3. Nothing else allocates ids in that namespace while the guest runs, or it does so at fixed points. Hermit helper threads or a backend-resident runtime that create tasks inside the namespace at varying times would break this.

Callers use the result in futex words, signals, robust lists, and diagnostics. A wrong id is therefore not just a cosmetic difference: a host id in a futex word or `tgkill` target is observable control flow.

Reconciliation with the 2026-08-07 draft (https://github.com/rrnewton/hermit/issues/1949), which proposed returning "the deterministic logical thread ID" with "explicit translation at all host-facing boundaries": at this SHA no translation is needed, because Detcore's logical id is the namespaced kernel id. `init_thread_state` builds each `DetTid` with `DetPid::from_raw(tid.into())` (`detcore/src/lib.rs:1494` and `1500`), under `// TODO(T78538674): virtualize tid, extend tid<=>dettid mapping here.` (`detcore/src/lib.rs:1492`). The same holds for the pid (`detcore/src/lib.rs:1069`) and for a new thread's id (`detcore/src/lib.rs:1661`).
The draft's translation layer would be needed in two cases: if that TODO lands and logical ids stop equaling kernel ids, or to make ids deterministic on a backend that does not give the guest a fresh PID namespace, which owner decision 3 does not require. In both cases forwarding leaks an untranslated id. Until then, forwarding returns exactly the logical id, and translation would be an identity map.

### Nondeterminism sources

- No fresh PID namespace: in-process backends have no namespace at all (`detcore/src/syscall_classification.rs:1007`-`1008`), so `gettid` returns a host id. The `pid-tid-identity` cell disables DBT because "DBT was observed emitting a raw host tid in DETLOG records" (`tests/e2e/manifests/determinism-stress-c.yaml`, cell at line 254). `--no-namespace` also drops the PID namespace (`hermit-cli/src/bin/hermit/run.rs:4536` to `:4542`). Both are reduced fidelity (see Hermit design).
- Tasks created inside the guest's namespace by something other than the guest, at varying times (condition 3 above)
- `pid_max` wraparound: a guest that creates more than `pid_max` tasks wraps. `pid_max` is a host setting, but the wrap point depends only on it, so it repeats on one host.
- `ns_last_pid` writes and `clone3` `set_tid`: guest actions, so they repeat. They are listed because they make F6 depend on guest-controlled state.

### Cross-host variation

- Host `kernel.pid_max` (commonly 32768 or 4194304): the guest's PID namespace wraps at this host value, so a guest that creates more tasks than `pid_max` sees reused small ids at a point that repeats on one host but moves between hosts (F6). Detcore forwards the id unchanged (`detcore/src/lib.rs:2843`).

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `model`. The typed dispatch arm `Syscall::Getpid(_) | Syscall::Gettid(_)` at `detcore/src/lib.rs:7693` delegates the call once, through the normal observation and posthook path, then projects the result with `project_own_getter_result` (`detcore/src/syscalls/namespace.rs:2025`): a positive answer outside a nested PID namespace is replaced by the shared Detcore identity (`own_detpid` for `getpid`, the thread's `dettid` for `gettid`); nonpositive answers and every error keep the delegated value. This supersedes the forwarding design analysed below, which remains as the record of why a forwarded answer was repeatable in the default ptrace namespace but could not be Detcore's identity on backends whose guest runs as a host process.

Target policy: `passthrough`. The code already does this.

- `gettid` is classified `PassThrough` at `detcore/src/syscall_classification.rs:705` (hermit f4055eb9cae4), in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. That block's comment (`detcore/src/syscall_classification.rs:691`-`694`) calls these calls conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions, and the identity group is under `TODO-HUMAN-REVIEW(#663)` (`detcore/src/syscall_classification.rs:699`-`700`).
  The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The census list holds it at `detcore/src/syscall_classification.rs:1710`.
- In run mode the default subscription intercepts every syscall (`detcore/src/lib.rs:1083`-`1085`: "interception is what charges generic syscall logical time"). Even the `--passthru-opt` allow-list keeps `gettid` subscribed (`detcore/src/lib.rs:1219`, next to `getpid` at `1218`). Each call is charged `FAST_NS` (250 ns, `detcore/src/syscall_time.rs:37`-`38` and `13`). Code facts show `subscribed_record_replay: true`.
- The robust-futex owner check depends on the identity. `detcore/src/syscalls/threads.rs:1106`-`1117` explains that it compares the lock word with `dettid` only because "`DetTid` currently *is* the raw namespaced `Tid`", and names `hermit-cli/tests/robust_futex_owner_death.rs` as the tripwire that fails the moment the two diverge. That comment cites `lib.rs:1257`, which is stale: the line is `detcore/src/lib.rs:1494` at this SHA.
- Target (owner decision 15, 2026-10-04): the guest loses the `clone` controls over id allocation. `clone3` with a non-empty `set_tid` and `clone`/`clone3` with `CLONE_NEWPID` get `EPERM`, as the `clone3` and `clone` bodies specify, so ids stay in the one container namespace and follow creation order (F5, F6). Today `handle_clone_family` injects the guest's flags and `clone_args` unchanged (`detcore/src/syscalls/threads.rs:924`), so by this reading both reach the kernel, where the guest's root in its user namespace holds the capability they need.

Why passthrough, not `model`: Detcore has no logical id that differs from the kernel's. A `model` handler would return the same number while adding a second source of truth that every `CLONE_*SETTID` write, futex word and `tgkill` target would also have to translate. Revisit this policy when T78538674 (the TODO at `detcore/src/lib.rs:1492`) is implemented, or when a backend without a PID namespace is meant to pass the identity cells.

Existing evidence (enabled tests; this body does not claim they pass at this SHA):

- `determinism-stress-c/pid-tid` (`tests/e2e/manifests/determinism-stress-c.yaml:196`, `verify` `ci: true`, ptrace only) runs `tests/e2e/determinism-stress/guests/pid_tid.c`. Threads record `syscall(SYS_gettid)` (`pid_tid.c:22`), the root prints its pid, ppid and tid (`pid_tid.c:27`-`31`), and the test asserts that each thread's pid equals `getpid()` while its tid differs (`pid_tid.c:41`-`42`). A forked child's tid must equal its pid (`pid_tid.c:62`, `74`-`75`).
- `determinism-stress-c/pid-tid-identity` (`tests/e2e/manifests/determinism-stress-c.yaml:254`, `verify` `ci: true`, ptrace only) runs `tests/e2e/determinism-stress/pid_tid_identity.c`. It prints a non-main thread's `gettid` (`pid_tid_identity.c:80`) and the main thread's (`pid_tid_identity.c:93`), and branches on `tid == pid` (`pid_tid_identity.c:98`-`106`). That branch only prints, so it is checked by run-to-run comparison and not asserted.
- `hermit-cli/tests/fixtures/nonleader_exec.c:229` checks `my_pid() == leader && my_tid() == leader` after a non-leader `execve`. It is driven by `run_ptrace_nonleader_exec_preserves_identity_and_time` (`hermit-cli/tests/cli.rs:5520`).

- Reduced fidelity (owner decision 3): under `--no-namespace` (no PID namespace, `hermit-cli/src/bin/hermit/run.rs:4542`; refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on in-process backends such as DBT, `gettid` returns a host thread id that changes on every run, and Detcore's `DetTid` is that host id too.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, assert that the main thread's `gettid() == getpid()`, that each of four `pthread` threads has a distinct `gettid()` different from `getpid()` and equal to the id `pthread_create` stored through `CLONE_PARENT_SETTID`, and that a forked child's `gettid()` equals the pid `fork` returned. Print every id so the two runs are compared.
  Existing coverage, for F2 and F3: `tests/e2e/determinism-stress/guests/pid_tid.c:41`-`42` and `74`-`75` in cell `determinism-stress-c/pid-tid`. F1 is printed and branched on at `tests/e2e/determinism-stress/pid_tid_identity.c:98`-`106` but not asserted, so T1 still needs an assertion for F1.
- T2 (covers F4): a non-leader thread calls `execve`. The new image asserts that `gettid() == getpid() ==` the old leader's id, and that `tgkill(leader, leader, 0)` succeeds. Existing coverage: `hermit-cli/tests/fixtures/nonleader_exec.c:229` via `hermit-cli/tests/cli.rs:5520`.
- T3 (covers F5): assert that the guest's root process reads `getpid() == gettid() == 3`, Hermit's documented namespace convention (`detcore/src/consts.rs:29`-`30`), and that `/proc/self/status` `Pid:` agrees. The test is a bracket for a change in that convention.
- T4 (covers F6): create and join 64 threads one after another, recording each `gettid()`. Assert that the sequence is identical across both `--verify` runs and strictly increasing until reuse. Then create threads while others exit, and assert that any reuse of a reaped id happens at the same point in both runs.
- T5 (covers F2, F6): use returned ids with `tgkill`, a `FUTEX_LOCK_PI` owner word, and robust-list cleanup at thread death, and check that the owner-died bit names the dead thread's `gettid()`. `hermit-cli/tests/robust_futex_owner_death.rs` already exercises robust-list owner death. It should be checked as the coverage for this item before writing a new test.
- T6 (covers none): withdrawn under owner decision 3. Configurations without a fresh PID namespace are reduced fidelity and do not count toward `done`, which is judged under `hermit run --strict` on ptrace in its container; `--strict` already refuses `--no-namespace`, pinned by `strict_rejects_every_route_to_host_networking` (`hermit-cli/src/bin/hermit/run.rs:2293`). F5 stays covered by T3.
- T7 (covers F6): under `hermit run --verify` on ptrace, a raw `SYS_clone3` with `CLONE_VM|CLONE_THREAD|CLONE_SIGHAND` and `set_tid` holding one free id returns -1 with `EPERM` (owner decision 15), and the next `pthread_create` gets the id the sequence predicts, identical in both runs. Fails at this SHA if the kernel accepts the `set_tid` (`detcore/src/syscalls/threads.rs:924`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. gettid is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1949: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional; target_policy investigate -> passthrough because Detcore's logical tid is the raw namespaced kernel tid (detcore/src/lib.rs:1492-1500), so forwarding already returns the deterministic id under a fresh PID namespace; reconciled the 2026-08-07 translation draft (needed only once T78538674 lands or for a backend without a PID namespace); listed 6 behaviours and 6 tests citing the pid-tid and pid-tid-identity cells and nonleader_exec.c:229; tracking_status stays todo; open question on configurations without a fresh PID namespace
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host kernel.pid_max sets where the guest's PID namespace wraps and reuses ids); no U items; open question untouched (pending --no-namespace decision)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (host tids under --no-namespace, confirmed to drop the PID namespace at hermit-cli/src/bin/hermit/run.rs:4542, and on in-process backends); target_policy unchanged (passthrough); T6 narrowed to covers none (bracket for namespace-less configurations withdrawn, F5 stays covered by T3); no new F/T ids; removed the open question on configurations without a fresh PID namespace and its Open questions heading
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 15 applied (clone3 set_tid and CLONE_NEWPID get EPERM, so ids follow creation order in the one container namespace; today both are injected unchanged); target_policy stays passthrough; F6 narrowed (set_tid steering refused); new T7; no open question removed


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: passthrough -> determinized (own-ID getters now project from shared Detcore state); Summary and Hermit design updated, target_policy passthrough -> model.

## Discussion

[Start a discussion about `gettid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+gettid&body=Discussion+for+the+%60gettid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgettid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60gettid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: gettid" (https://github.com/rrnewton/hermit/issues/1949), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy text (2026-08-07)

Legacy acceptance text: "Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy."

Legacy required tests, rewritten above as T1-T6:

- Check main-thread and child-thread IDs under several deterministic clone schedules.
- Use returned TIDs with tgkill, futex ownership, robust-list cleanup, and wait/exit paths.
- Assert stable IDs across strict repeats, record/replay, and all supporting backends.
- Bracket against a fixture that leaks a host TID or aliases two live logical threads.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

gettid is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
