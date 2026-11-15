---
schema: syscall-pages/v1
syscall: unshare
nr_x86_64: 272
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/507
  - https://github.com/rrnewton/hermit/issues/528
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+unshare&body=Discussion+for+the+%60unshare%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funshare.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60unshare%60%2C+or+in+what+the+page+says%3F%0A
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

`unshare(flags)` gives the calling process private copies of selected resources it shares with others: its file descriptor table, filesystem context, System V semaphore undo list, and its namespaces (mount, user, PID, network, IPC, UTS, cgroup, time). Hermit refuses every call with a fixed `EPERM`, including `unshare(0)`, and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4), so the guest stays in the namespaces Hermit's container set up.

Manual reference: [unshare(2)](https://man7.org/linux/man-pages/man2/unshare.2.html).

## Linux semantics

The kernel (`kernel/fork.c`, `ksys_unshare`) first rejects unknown bits with `EINVAL`, then adds implied flags: `CLONE_NEWUSER` implies `CLONE_THREAD` and `CLONE_FS`, `CLONE_NEWNS` implies `CLONE_FS`, and `CLONE_VM`/`CLONE_SIGHAND` imply each other and `CLONE_THREAD`. The thread-related flags succeed only if the caller is single-threaded and its `mm` and signal handlers are not shared, otherwise `EINVAL`. Each requested resource is then copied; on success the caller holds the copies and its parent and siblings keep the originals.

Process-local flags (`CLONE_FILES`, `CLONE_FS`, `CLONE_SYSVSEM`) need no privilege. A new user namespace needs no privilege either (subject to the `user.max_user_namespaces` limit and LSM policy) and makes the caller fully privileged inside it. Every other new namespace needs `CAP_SYS_ADMIN` in the caller's user namespace, which Hermit's ptrace guest has as root of its own user namespace (`detcore/src/syscalls/misc.rs:672`). A new PID or time namespace applies only to children created afterwards.

`unshare(0)` does nothing and returns 0, as both host probes show.

### Flags and options

- F1: `flags = 0`: no-op, returns 0.
- F2: `CLONE_FILES`: stop sharing the file descriptor table (after a `clone(CLONE_FILES)`); unprivileged.
- F3: `CLONE_FS`: stop sharing root, current directory and umask; unprivileged.
- F4: `CLONE_SYSVSEM`: stop sharing the System V semaphore undo list (the caller's undo adjustments are applied first); unprivileged.
- F5: `CLONE_THREAD`, `CLONE_SIGHAND`, `CLONE_VM`: accepted as no-ops for a single-threaded caller that shares nothing, otherwise `EINVAL`.
- F6: `CLONE_NEWUSER`: new user namespace with the caller as owner. `EINVAL` if multi-threaded, `EPERM` if the caller is in a chroot, `ENOSPC`/`EUSERS` at the namespace count or nesting limit (32 levels).
- F7: `CLONE_NEWNS`: new mount namespace, a copy of the current mount table; mounts locked against the parent namespace stay locked.
- F8: `CLONE_NEWPID` and `CLONE_NEWTIME`: the caller stays where it is; the next child is PID 1 of the new PID namespace or uses the new time-namespace offsets.
- F9: `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWNET`, `CLONE_NEWCGROUP`: the caller moves into a new namespace of that kind at once (fresh hostname, empty System V IPC set, network namespace with only `lo` down, or cgroup root at its current cgroup).
- F10: errors: unknown bits give `EINVAL`; missing capability gives `EPERM`; namespace limits give `ENOSPC`; allocation failure gives `ENOMEM`.
- F11: Hermit's refusal (owner decisions 6 and 15): `unshare(0)` returns 0, and every other `flags` value returns `EPERM` before any check of the bits, unknown bits and multi-threaded callers included. Nothing is forwarded to the kernel, and the caller's descriptors, filesystem context and namespaces are unchanged.

## Determinism analysis

`linux_determinism: conditional`, kept from both legacy issues. F1 is a deterministic no-op, and F2-F5 only change what the caller shares, so with fixed inputs they repeat. The namespace flags F6-F9 change the environment every later call observes: new namespaces get fresh host-allocated inode numbers (visible through `/proc/self/ns/*`), a new network namespace has different interfaces, a new PID namespace changes the PIDs children see, and success depends on host configuration (kernel config, `user.max_*_namespaces` limits, LSM policy, privilege). A new namespace would also replace parts of the environment Hermit is determinizing.

### Nondeterminism sources

- Kernel namespace support, the `user.max_*_namespaces` limits and LSM policy
- Capabilities and the user-namespace mappings the host provides
- Thread count and resource sharing, which decide `EINVAL` for F5 and F6
- Host-allocated namespace inode numbers, and the contents of the new namespaces (mount table copy, network devices, cgroup root)

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `unshare` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724). The comment there says the deliberate refusal covers the unprivileged sub-modes too, naming user-namespace `unshare`, because the container pins the guest's namespaces for the run.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm does not read `flags`, so `unshare(0)` also fails. It is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test that names it is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership and runs no guest. `hermit-cli/tests/rr_suite.rs:540` declares `rr_test!(rr_unshare, "unshare", 0, &[])`, but every `rr_test!` is `#[ignore]` (`hermit-cli/tests/rr_suite.rs:230-233`), so it does not run.

Why `refuse`:

- The namespace flags F6-F9 would replace the namespaces Hermit set up and bring in host-allocated identities and host network or cgroup state that Detcore does not model.
- Passing through the process-local flags is not free either. Detcore keeps per-thread file metadata that is shared after `clone(CLONE_FILES)` and forked otherwise (`detcore/src/lib.rs:1544-1550`); a forwarded `unshare(CLONE_FILES)` would split the kernel's descriptor table without Detcore forking its copy.
- A fixed `EPERM` is deterministic and matches what an unprivileged process sees on a host that forbids user namespaces, which programs already handle.

Known differences from Linux:

- `unshare(0)` and the unprivileged process-local flags F2-F5 succeed on Linux and fail with `EPERM` under Hermit today. The target returns 0 for `unshare(0)` (below); F2-F5 keep `EPERM`.
- Tools such as `unshare(1)`, bubblewrap and the Nix build sandbox fail at their first `unshare`.
- Hermit returns `EPERM` for unknown bits, where Linux gives `EINVAL` (F10).
- `clone` and `clone3` are not covered by this refusal: Detcore injects them with their original flags (`detcore/src/syscalls/threads.rs:924`), and nothing in `detcore/src` filters `CLONE_NEW*` bits. A guest can still create namespaces for a child that way. Owner decision 15 closes this: the `clone` and `clone3` bodies give `CLONE_NEW*` `EPERM` (https://github.com/rrnewton/hermit/issues/3676).

Target (owner decisions 6 and 15): keep the fixed `EPERM` for every request except `flags = 0`, which returns 0 without forwarding (F11).

- `unshare(0)` is the decision-6 exception: Linux allows it unprivileged. In `ksys_unshare` (`kernel/fork.c`), zero flags add no implied flags, pass `check_unshare_flags`, and copy nothing, so the kernel order owes no argument check and the answer is a plain 0. Today the arm at `detcore/src/lib.rs:2215-2219` returns `EPERM` without reading `flags`, so the target adds one test of the first argument ahead of that arm.
- Every other value keeps the one fixed `EPERM` before any check, as decision 6 says for refused calls. Unknown bits therefore give `EPERM`, not Linux's `EINVAL`, and a multi-threaded caller asking for `CLONE_NEWUSER` gets `EPERM`, not `EINVAL`. The namespace flags F6-F9 get `EPERM` under decision 15 as well.
- The process-local flags F2-F5 are also allowed unprivileged on Linux, but decision 6 names only `unshare(0)` as the exception for this call, so they stay refused.
- Fallback: glibc, musl and language runtimes do not call `unshare` on their own. Tools that need it, such as `unshare(1)`, bubblewrap and container runtimes, report the failure as they do on a host that forbids user namespaces.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F11): under `hermit run --verify` on ptrace, a guest calls `syscall(SYS_unshare, ...)` with 0, `CLONE_FILES`, `CLONE_FS`, `CLONE_SYSVSEM` and `CLONE_THREAD|CLONE_SIGHAND|CLONE_VM`. Assert 0 for `flags = 0` and -1 with `EPERM` for each of the others (at f4055eb9cae4 the first call also gives `EPERM`, so this test fails until the target is built), that both runs agree, and that a descriptor opened before the calls is still usable after them.
- T2 (covers F6, F7, F8, F9): the same guest tries `CLONE_NEWUSER`, `CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWTIME`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWNET` and `CLONE_NEWCGROUP`, each alone. Each returns `EPERM`, and `readlink` of every `/proc/self/ns/*` entry and `uname(2)` give identical results before and after.
- T3 (covers F10, F11): `unshare(0x80000000)` and a multi-threaded caller asking for `CLONE_NEWUSER`. Assert -1 with `EPERM` for both (Linux gives `EINVAL`; owner decision 6 keeps the fixed errno ahead of the argument checks) and that both `--verify` runs agree.
- T4 (covers F1, F6): `hermit record` then `hermit replay` a shell guest that runs `unshare --user true` and `unshare -r id -u` (util-linux) and prints exit status and stderr; the recording and the replay must match. Enabling the existing `rr_unshare` case in `hermit-cli/tests/rr_suite.rs:540` would be an alternative once its expected output is settled.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/507: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/528 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every unshare request, including flags 0, never forwarded); linux_determinism stays conditional; duplicate https://github.com/rrnewton/hermit/issues/528 merged (its NEEDS-HANDLER and this issue's NEEDS-INVESTIGATION both resolved as refuse for now); 10 flag behaviours and 4 tests listed; no coverage token because no running test calls unshare under a guest (hermit-cli/tests/rr_suite.rs:540 is #[ignore]), so tracking_status stays todo; open questions on modelling flags 0 and the process-local flags, and on clone/clone3 passing CLONE_NEW* flags through, under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 15 applied (unshare(0) returns 0 without forwarding, every other value keeps the fixed EPERM before any check, F2-F5 stay refused, the clone side door is closed in the clone and clone3 bodies); target_policy stays refuse; new F11; T1 and T3 now assert the target; removed all three open questions (flags 0 and F2-F5, clone side door, errno order) and the Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `unshare`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+unshare&body=Discussion+for+the+%60unshare%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funshare.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60unshare%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: unshare" (https://github.com/rrnewton/hermit/issues/507), legacy variant A1. Its comment history stays on that issue.

Merged duplicate: https://github.com/rrnewton/hermit/issues/528, filed 2026-07-24 as "Syscall classification: unshare" (legacy variant A1), retired into this issue. Its comment history (0 comments at migration) stays on that issue.

### Merge notes

- Both issues classified the call CONDITIONAL; the merged body keeps `conditional`.
- This issue recommended NEEDS-INVESTIGATION and the duplicate NEEDS-HANDLER ("success and the resulting environment require explicit policy and cannot be inherited blindly"). Both resolve to `refuse` at f4055eb9cae4; whether to narrow it was an open question, answered by owner decision 6 (2026-10-04): only `unshare(0)` succeeds.
- The duplicate's facts kept here: the full list of contexts the call can alter (mount, network, IPC, PID, time, UTS, user, cgroup, file table, filesystem context) is F2-F9; its nondeterminism sources (namespace support, capabilities, user mappings, thread count, resource sharing, host policy) are under Nondeterminism sources.
- Both issues report `unshare(0) = 0` on the host. That stays true for Linux (F1); under Hermit at f4055eb9cae4 the same call returns `EPERM`, which is a stated difference rather than a contradiction.
- Both issues said Hermit routed `unshare` through the UNCLASSIFIED temporary pass-through block (the duplicate at `dd60278f`). That was true when filed and is superseded by the refusal at f4055eb9cae4.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/507)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=unshare` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `unshare` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
unshare(0)                      = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (duplicate https://github.com/rrnewton/hermit/issues/528)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=unshare`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `unshare` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
unshare(0)                      = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
