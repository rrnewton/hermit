---
schema: syscall-pages/v1
syscall: setns
nr_x86_64: 308
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/433
  - https://github.com/rrnewton/hermit/issues/448
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setns&body=Discussion+for+the+%60setns%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetns.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setns%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`setns(fd, nstype)` moves the calling thread into an existing namespace named by a namespace file descriptor (from `/proc/<pid>/ns/*`), or, since Linux 5.8, into several namespaces of another process at once through a pidfd. Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4), so the guest stays in the namespaces Hermit's container set up.

Manual reference: [setns(2)](https://man7.org/linux/man-pages/man2/setns.2.html).

## Linux semantics

The kernel (`kernel/nsproxy.c`) looks up `fd` first: a bad descriptor gives `EBADF`, as both host probes show with `setns(-1, 0)`. A namespace file is joined as the namespace it names; `nstype` 0 accepts any kind, and a non-zero `nstype` must match it, otherwise `EINVAL`. A pidfd is joined through a mask of `CLONE_NEW*` bits in `nstype`, all applied together or none; the mask must be non-zero, and a target that has exited gives `ESRCH`. Any other kind of descriptor gives `EINVAL`.

Joining needs `CAP_SYS_ADMIN` over the target namespace's owning user namespace, and for most kinds also in the caller's own user namespace, otherwise `EPERM`. Hermit's ptrace guest is root only of its own user namespace (`detcore/src/syscalls/misc.rs:672`), so it could join namespaces that namespace owns (for example ones created by its own children) but not the host's.

Kind-specific rules:

- Mount namespace: also needs `CAP_SYS_CHROOT`; the caller must not share its filesystem context (`CLONE_FS`) with another process, otherwise `EINVAL`; root and current directory are reset to the target namespace's root.
- User namespace: the caller must be single-threaded and not share its filesystem context, and cannot re-enter its own current user namespace (`EINVAL`); it gains full capabilities in the target.
- PID and time namespaces: only children created afterwards are placed in the target.
- Network, UTS, IPC and cgroup namespaces: take effect for the calling thread at once.

The legacy note that this is used by container tooling (`nsenter`, `ip netns exec`, container runtimes) holds.

### Flags and options

- F1: namespace file descriptor with `nstype = 0`: join whatever namespace kind `fd` refers to.
- F2: namespace file descriptor with one `CLONE_NEW*` value in `nstype`: join only if the kinds match, otherwise `EINVAL`.
- F3: pidfd with a non-zero mask of `CLONE_NEWUSER`, `CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWTIME`, `CLONE_NEWNET`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWCGROUP` (Linux 5.8 and later): join that process's namespaces atomically. A zero mask gives `EINVAL`; an exited target gives `ESRCH`.
- F4: mount namespace target: needs `CAP_SYS_ADMIN` and `CAP_SYS_CHROOT`, an unshared filesystem context, and resets root and current directory.
- F5: user namespace target: single-threaded caller only, cannot rejoin its own namespace, grants full capabilities in the target.
- F6: PID or time namespace target: affects children created afterwards, not the caller.
- F7: network, UTS, IPC or cgroup namespace target: affects the calling thread at once.
- F8: errors: `EBADF` for a bad descriptor (both host probes), `EINVAL` for a descriptor that is neither a namespace file nor a pidfd or for a kind mismatch, `EPERM` without the capabilities, `ENOMEM`.
- F9: Hermit's refusal (owner decisions 6 and 15): every call returns `EPERM` before `fd` or `nstype` is looked at, and nothing is forwarded to the kernel. The caller's namespaces, root and current directory are unchanged.

## Determinism analysis

`linux_determinism: conditional`, kept from both legacy issues. With a bad descriptor or without privilege it fails with an error that depends only on the arguments, so it repeats; the probes show `EBADF` with no state change. On success it replaces part of the thread's kernel-visible environment: mount table, network devices, PIDs seen by later children, hostname, IPC objects, cgroup view, clock offsets, credentials. All of that is the contents of a namespace that exists outside the call, so later observations depend on host namespace state and on whoever else is changing it.

### Nondeterminism sources

- The identity of the namespace behind the descriptor, and the host namespaces it refers to
- Capabilities, and the ancestry of user namespaces that decides which namespaces the caller may join
- The contents of the joined namespaces (mounts, network configuration, processes, hostname, IPC objects), which other processes may change concurrently
- Thread count and filesystem-context sharing, which decide `EINVAL` for F4 and F5

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `setns` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724). The comment there says the container pins the guest's namespaces for the run and that entering a namespace is global kernel state.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm reads neither argument and is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership and runs no guest.

Why `refuse`:

- A joined namespace is external state Detcore does not model: its mount table, network devices, PIDs and IPC objects are whatever exists on the host.
- The guest can obtain pidfds under Hermit (`pidfd_open` has a handler, `detcore/src/lib.rs:2591`), so F3 would be reachable if the call were forwarded.
- The duplicate's note that "fd must be virtualized" applies: a `model` policy would need Detcore to know which namespace each descriptor names, and nothing at this SHA tracks that. With refusal no namespace descriptor is ever acted on.
- `model` would only become meaningful if Hermit also let the guest create namespaces (owner decision 15 keeps it from doing so).

Known differences from Linux:

- On Linux the guest could join namespaces its own user namespace owns, for example ones a child created. Under Hermit every request fails with `EPERM`; `nsenter(1)` and container runtimes see that error.
- Hermit returns `EPERM` before looking at `fd`, so the host probe's `setns(-1, 0)` gives `EPERM` under Hermit instead of `EBADF`, and a kind mismatch gives `EPERM` instead of `EINVAL` (F8).
- `clone` and `clone3` pass namespace flags through (`detcore/src/syscalls/threads.rs:924`), so a guest can still create namespaces for a child but cannot then `setns` into them. Owner decision 15 closes this: the `clone` and `clone3` bodies give `CLONE_NEW*` `EPERM` (https://github.com/rrnewton/hermit/issues/3676), so the guest has no namespace of its own making to join.

Target (owner decisions 6 and 15): today's code is the target. `detcore/src/lib.rs:2215-2219` returns the one fixed `EPERM` before reading either argument (F9). Decision 6 keeps that errno ahead of the argument checks Linux makes first in `SYSCALL_DEFINE2(setns)` (`kernel/nsproxy.c`): the descriptor lookup (`EBADF`), then the namespace-file or pidfd test and the `nstype` checks (`EINVAL`). Decision 6's exception does not apply, because Linux never lets a caller join a namespace without `CAP_SYS_ADMIN` over it. Decision 15 names this call among those whose refusal the clone filter protects. Fallback: glibc and language runtimes do not call `setns`; `nsenter(1)`, `ip netns exec` and container runtimes report the `EPERM`, as they do for an unprivileged user.

## Recommended tests

- T1 (covers F1, F2, F4, F5, F6, F7, F9): under `hermit run --verify` on ptrace, a guest opens each `/proc/self/ns/*` entry (`mnt`, `user`, `pid`, `time`, `net`, `uts`, `ipc`, `cgroup`) and calls `syscall(SYS_setns, fd, 0)` and `syscall(SYS_setns, fd, <matching CLONE_NEW*>)`. Each must return -1 with `EPERM`, and `readlink` of every `/proc/self/ns/*` entry, `uname(2)` and `getcwd(3)` give identical results before and after.
- T2 (covers F3): the guest obtains a pidfd for itself with `pidfd_open(getpid(), 0)` and calls `setns(pidfd, CLONE_NEWNET|CLONE_NEWUTS)` and `setns(pidfd, 0)`. Record the results (`EPERM` for both at f4055eb9cae4, where Linux would give success-or-`EPERM` and `EINVAL`) and assert both `--verify` runs agree.
- T3 (covers F8, F9): `setns(-1, 0)` (the host probe), and `setns` on a regular file descriptor. Assert -1 with `EPERM` for both (Linux gives `EBADF` and `EINVAL`; owner decision 6 keeps the fixed errno ahead of the argument checks) and that both runs agree; then `hermit record` and `hermit replay` the same guest and require identical output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/433: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/448 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every setns request, never forwarded); linux_determinism stays conditional; duplicate https://github.com/rrnewton/hermit/issues/448 merged (its NEEDS-INVESTIGATION and this issue's NEEDS-HANDLER both resolved as refuse); 8 flag behaviours and 3 tests listed; no coverage token because no test runs a guest that calls setns, so tracking_status stays todo; open questions on errno order and on clone/clone3 namespace flags under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 15 applied (fixed EPERM before the fd lookup stays, no unprivileged exception; the clone side door is closed in the clone and clone3 bodies); target_policy stays refuse; new F9; T1 and T3 now cover F9 and T3 asserts EPERM; removed both open questions (errno order, clone namespace flags) and the Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setns`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setns&body=Discussion+for+the+%60setns%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetns.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setns%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setns" (https://github.com/rrnewton/hermit/issues/433), legacy variant A1. Its comment history stays on that issue.

Merged duplicate: https://github.com/rrnewton/hermit/issues/448, filed 2026-07-24 as "Syscall classification: setns" (legacy variant A1), retired into this issue. Its comment history (0 comments at migration) stays on that issue.

### Merge notes

- Both issues classified the call CONDITIONAL; the merged body keeps `conditional`.
- This issue recommended NEEDS-HANDLER ("must be refused or explicitly modeled") and the duplicate NEEDS-INVESTIGATION. Both resolve to `refuse`, which the code at f4055eb9cae4 does.
- The duplicate's facts kept here: the call reassociates the thread with a host namespace, which is host-configuration dependent and usually privileged; it fails deterministically without privilege or with a bad descriptor; the namespace descriptor would have to be virtualized for any model (Hermit design); it is used by container tooling (Linux semantics).
- The duplicate said it "usually requires CAP_SYS_ADMIN". The merged body is more precise: `CAP_SYS_ADMIN` over the target's owning user namespace, plus `CAP_SYS_CHROOT` for mount namespaces (F4).
- Both issues said Hermit routed `setns` through the UNCLASSIFIED temporary pass-through block (this issue at `dd60278f`). That was true when filed and is superseded by the refusal at f4055eb9cae4.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/433)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setns`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setns` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setns(-1, 0)                    = -1 EBADF (Bad file descriptor)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (duplicate https://github.com/rrnewton/hermit/issues/448)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setns` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `setns` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setns(-1, 0)                    = -1 EBADF (Bad file descriptor)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
