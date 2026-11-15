---
schema: syscall-pages/v1
syscall: getuid
nr_x86_64: 102
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getuid&body=Discussion+for+the+%60getuid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetuid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getuid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`getuid()` returns the real user ID of the calling thread. It takes no arguments and cannot fail. Hermit never forwards it: Detcore answers a constant 0 (`detcore/src/lib.rs:2760`-`2763` at hermit f4055eb9cae4), the fixed virtual-root identity that the credential setters (no-ops) and the other getters share. The target is a per-process credential record that the setters update (owner decision 4). See [getuid(2)](https://man7.org/linux/man-pages/man2/getuid.2.html).

## Linux semantics

The kernel returns `from_kuid_munged(current_user_ns(), current_uid())`: the real uid in the calling thread's credentials, translated into the caller's user namespace. The call has no side effects and no error return.

The value is inherited across `fork`, `clone` and `execve`. Only an explicit credential change alters it: `setuid` by a caller with `CAP_SETUID`, `setreuid` or `setresuid`. Executing a set-user-ID binary changes the effective and saved uids, never the real uid. Credentials belong to each thread. The raw setter syscalls change only the calling thread; glibc's wrappers repeat the change on every thread through an internal signal.

### Flags and options

- F1: the return value is the real uid of the calling thread's credentials, as seen from the caller's user namespace. The call never fails and has no side effects.
- F2: user-namespace translation: a real uid with no mapping in the caller's namespace reads as the overflow uid (`/proc/sys/kernel/overflowuid`, 65534 by default). One way to reach it is a fresh `CLONE_NEWUSER` namespace before its `uid_map` is written.
- F3: changes over the process's life: `setuid` by a privileged caller, `setreuid` and `setresuid` change the real uid; `fork` and `clone` copy it; `execve` keeps it, including for a set-user-ID binary.
- F4: thread scope: a raw `SYS_setuid`/`SYS_setreuid`/`SYS_setresuid` changes only the calling thread, so afterwards two threads of one process can report different `getuid()` values. The glibc wrappers apply the change to every thread.

## Determinism analysis

`linux_determinism: conditional`. The value is a pure function of the caller's credentials and its user-namespace mapping, and nothing in the call races with anything else. It repeats when every run starts with the same identity and mapping and makes the same credential changes in the same order.

### Nondeterminism sources

- The identity that launched the program. Natively, the real uid is whoever ran it, so the value differs between users and hosts.
- The user-namespace mapping (F2), which decides the translated value and the overflow uid case.
- Across threads (F4): whether a thread reads its uid before or after another thread's credential change reaches it depends on the schedule. Hermit's sequentialized schedule makes that order repeat, but it is still an input.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04). Detcore keeps a per-process credential record: real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets. `getuid` reports the record's real uid. The record starts with every id 0, `fork` and `clone` copy it, and `execve` keeps it. `setuid`, `setreuid` and `setresuid` update it and apply Linux's own permission rules against the record, so a process that has dropped every uid to 1000 gets `EPERM` from `setuid(0)`; their tracking bodies describe the update side. The record is not enforced: the kernel goes on checking the guest's real credentials, root in its user namespace.

What the code does at hermit f4055eb9cae4, where no record exists yet:

- The typed dispatch arm returns `Ok(0)` for `getuid`, `geteuid`, `getgid` and `getegid` without reading guest state (`detcore/src/lib.rs:2760`-`2763`). Its comment (`detcore/src/lib.rs:2752`, tagged `TODO-HUMAN-REVIEW(#1549)`) says the answer is never forwarded to the host. The model replaces the constant with the record's real uid and keeps the "never forwarded" property.
- `getuid` is classified `Determinized` at `detcore/src/syscall_classification.rs:669`. The comment at `detcore/src/syscall_classification.rs:657`-`668` gives the reason: the query family used to be passed through and read 0 only because the ptrace backend runs the guest in a `CLONE_NEWUSER` namespace that maps the launching user to 0. An in-process backend has no such namespace and leaked the host uid. The container setup is `.map_root()` at `hermit-cli/src/bin/hermit/run.rs:4631`.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`) asserts the classification of the four getters plus `getresuid`/`getresgid` (`detcore/src/syscall_classification.rs:2309`). It does not see the dispatch arm's value.
- Nothing updates an identity today: the setters are no-ops that return 0 (`is_credential_identity_noop_syscall`, `detcore/src/syscall_classification.rs:974`, applied at `detcore/src/lib.rs:2272`-`2276`), and `getresuid` writes 0 to each result pointer (`detcore/src/syscalls/misc.rs:679`).
- Reduced fidelity (owner decision 3): under `--no-namespace` (`--core-only`) or a backend without a user namespace, `getuid` still reads the Detcore value while the kernel checks and reports the launching host user, so kernel-filled observers disagree with it: `stat` reports the real host uid (doc comment at `detcore/src/syscall_classification.rs:1002`), and so does `si_uid` in a self-sent signal (compared with `getuid()` at `tests/c/sigpipe_siginfo.c:39`). No fidelity is required there.

Why `model` and not the alternatives:

- `emulate`, the constant 0 of today, makes a privilege drop invisible: after `setuid(1000)` the guest still reads 0, and a daemon that checks its drop took effect aborts.
- `passthrough` is what the code moved away from: the answer then depended on whether the backend created a user namespace and on who launched Hermit.
- `refuse` would break every program that asks who it is.
- The accepted cost of `model` is that the modelled uid is not enforced: after `setuid(1000)` the guest can still open root-only files.

### Differences from Linux

- **Today, credential changes are invisible (F3).** At hermit f4055eb9cae4, after `setuid(1000)` or `setresuid(1000, 1000, 1000)`, which Hermit accepts as no-ops, `getuid()` still returns 0. Natively a root process would read 1000. The model removes this difference.
- **No enforcement (target).** After a modelled drop to uid 1000 the kernel still applies root's permissions inside the container, and kernel-filled observers (the owner of a new file, `si_uid`) still report 0. Before any credential change, `.map_root()` maps the launching user to 0, so the kernel's view and the record agree.
- **Thread scope (F4, target).** The record is per process, so a raw `SYS_setresuid` in one thread changes what every thread reads; on Linux only the calling thread changes. The glibc wrappers change every thread on Linux too, so for them the two agree.
- **No overflow uid (F2).** Hermit reports the record where Linux would return 65534. `unshare` is refused with `EPERM` (`is_mount_ns_admin_refused_syscall`, `detcore/src/syscall_classification.rs:951`), so the guest cannot reach this case through `unshare`. `clone` and `clone3` with `CLONE_NEWUSER` do create the namespace today: `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`) injects the flags unchanged (`detcore/src/syscalls/threads.rs:924`; https://github.com/rrnewton/hermit/issues/3676), and the child still reads the record, because `getuid` is never forwarded.
- Target (owner decision 15, 2026-10-04): `clone` and `clone3` with any `CLONE_NEW*` flag return `EPERM`, as `unshare` already does, so a guest cannot enter a fresh user namespace and F2 cannot arise under Hermit. A program that tries to sandbox itself in a user namespace (a browser or container tool) gets the `EPERM` it already handles from unprivileged Linux.
- **`/proc/<pid>/status` is not covered.** Its `Uid:` line shows the kernel's credentials, so it disagrees with the record after a change. That stays an open item for `/proc` virtualization.

## Recommended tests

- T1 (covers F1): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_getuid)` returns 0, the record's starting value, in the first process, in forked children and after an `execve`, identical across runs.
  Existing partial coverage: `tests/e2e/determinism-stress/fork_tree.c:39` calls `SYS_getuid` 25 times in each of its 13 processes, but only checks `uid < 0` (`tests/e2e/determinism-stress/fork_tree.c:44`), not the value. Its cell `determinism-stress-c/fork-tree` (`tests/e2e/manifests/determinism-stress-c.yaml:4`) runs verify on ptrace in CI (`tests/e2e/manifests/determinism-stress-c.yaml:23`).
- T2 (covers F2): inside the guest, raw `clone(CLONE_NEWUSER | SIGCHLD)` and `clone3` with `CLONE_NEWUSER` both return `EPERM` (owner decision 15), identically across runs, and `getuid()` in the caller still reads the record. Natively the child would read 65534 before any `uid_map` write. Today the clone succeeds and the child reads the record, so the `EPERM` assertion fails until the clone-flag filter lands.
- T3 (covers F3): read-back through the record under `--strict` on ptrace. `setresuid(1000, 1001, 1002)` returns 0 and `getuid()` reads 1000; `setreuid(1001, -1)` (unprivileged, allowed because 1001 is the effective uid) makes it read 1001; after that full drop, `setuid(0)` fails with `EPERM` and `getuid()` still reads 1001. A forked child and an `execve`d image read the same value. In a fresh process, `setuid(1000)` called as root sets all three uids, so `getuid()` reads 1000 and a following `setuid(0)` fails with `EPERM`.
- T4 (covers F4): in a process with two threads, one thread issues raw `syscall(SYS_setresuid, 1000, 1000, 1000)`. Then both threads read `getuid()`. Assert that both read 1000, the per-process record's answer, identically across runs; the test lists the other thread's value as an accepted difference from Linux, where it stays 0.
- T5 (covers F1): consistency with kernel-filled observers in the strict ptrace container. Before any credential change, compare `getuid()` with the owner of a newly created file (`stat`), the `Uid:` line of `/proc/self/status` and `si_uid` of a self-sent signal, and assert they agree; after `setresuid(1000, 1000, 1000)`, record which of them disagree with the record (all three are expected to stay 0).
  Existing partial coverage: `tests/c/sigpipe_siginfo.c:39` requires `si_uid == getuid()` (cell `c-programs/sigpipe-siginfo`, `tests/e2e/manifests/c-programs.yaml:8207`, verify ci true at `tests/e2e/manifests/c-programs.yaml:8234`), under the default namespace only.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (the dispatch arm returns the fixed virtual-root uid 0 and never forwards, matching the no-op setters); linux_determinism stays conditional (depends only on the launching identity and user-namespace mapping); 4 flag behaviours and 5 tests listed; no coverage token because fork_tree.c checks only uid >= 0 and nothing asserts the value, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, read-back of setuid/setreuid/setresuid, Linux permission rules against the record, no enforcement); Hermit design rewritten with a reduced-fidelity line for --no-namespace and namespace-less backends; T1 and T5 narrowed to the strict ptrace container, T3 and T4 now assert read-back (1000/1001, EPERM after a full drop, per-process thread scope); no new F/T ids; open question on modelling the credential family removed (answered by decision 4)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 15 applied (clone/clone3 with CLONE_NEWUSER get EPERM, so the overflow-id case F2 cannot arise; recorded that today the flags are injected unchanged at threads.rs:924; T2 now asserts EPERM); target_policy stays model; no new F/T ids; no open question removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getuid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getuid&body=Discussion+for+the+%60getuid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetuid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getuid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getuid`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The query family was moved from pass-through to the constant 0 under the marker `TODO-HUMAN-REVIEW(#1549)` (https://github.com/rrnewton/hermit/issues/1549); the setter no-ops under `TODO-HUMAN-REVIEW(#787)` (https://github.com/rrnewton/hermit/issues/787).

The portable compat corpus runs `/usr/bin/id -u` (`tests/e2e/manifests/compat.yaml:71`) and `/usr/bin/whoami` (`tests/e2e/manifests/compat.yaml:153`). Both print the effective uid, so they exercise `geteuid`, not `getuid`, and their comparator checks only that runs agree. `tests/c/syscall_file_metadata.c:145` passes `getuid()` to `fchown`/`fchownat`/`lchown`, which Detcore emulates as success for any owner (`is_ownership_change_noop_syscall`, `detcore/src/syscall_classification.rs:1025`), so it does not check the value either.
