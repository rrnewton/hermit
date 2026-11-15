---
schema: syscall-pages/v1
syscall: getegid
nr_x86_64: 108
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getegid&body=Discussion+for+the+%60getegid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetegid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getegid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`getegid()` returns the effective group ID of the calling thread, the gid the kernel uses for group permission checks and as the group of newly created files (unless the filesystem gid or a set-group-ID directory overrides it). It takes no arguments and cannot fail. Hermit never forwards it: Detcore answers a constant 0 (`detcore/src/lib.rs:2760`-`2763` at hermit f4055eb9cae4), the fixed virtual-root identity shared with the other credential getters and the no-op setters. The target is a per-process credential record that the setters update (owner decision 4). See [getegid(2)](https://man7.org/linux/man-pages/man2/getegid.2.html).

## Linux semantics

The kernel returns `from_kgid_munged(current_user_ns(), current_egid())`: the effective gid in the calling thread's credentials, translated into the caller's user namespace. The call has no side effects and no error return.

The effective gid is inherited across `fork` and `clone`. It changes through `setgid` (privileged or not), `setregid`, `setresgid`, and through `execve` of a set-group-ID binary. `setfsgid` and `setgroups` do not change it. Credentials belong to each thread: the raw setter syscalls change only the calling thread, and glibc's wrappers repeat the change on every thread.

### Flags and options

- F1: the return value is the effective gid of the calling thread's credentials, as seen from the caller's user namespace. The call never fails and has no side effects.
- F2: user-namespace translation: an effective gid with no mapping in the caller's namespace reads as the overflow gid (`/proc/sys/kernel/overflowgid`, 65534 by default), for example in a fresh `CLONE_NEWUSER` namespace before its `gid_map` is written.
- F3: credential changes: `setgid` (privileged or not), `setregid` and `setresgid` change the effective gid; `setfsgid` and `setgroups` do not; `fork` and `clone` copy it.
- F4: `execve` of a set-group-ID binary (mode bit `S_ISGID` with group execute) sets the effective gid to the file's group. The bit is ignored when the mount is `nosuid`, when `no_new_privs` is set, when the group has no mapping in the caller's user namespace, or when the process is traced by a tracer without the needed capability. Otherwise `execve` keeps the effective gid.
- F5: thread scope: a raw `SYS_setgid`/`SYS_setregid`/`SYS_setresgid` changes only the calling thread, so two threads of one process can report different `getegid()` values. The glibc wrappers apply the change to every thread.

## Determinism analysis

`linux_determinism: conditional`. The value is a pure function of the caller's credentials, its user-namespace mapping and, for F4, the group, mode and mount flags of each executed binary. Nothing in the call races. It repeats when every run starts with the same identity and mapping, executes the same files, and makes the same credential changes in the same order.

### Nondeterminism sources

- The identity that launched the program: natively the effective gid is the launching user's primary group.
- The user-namespace mapping (F2, and the mapping condition in F4).
- Group ownership, set-group-ID bits and mount flags of executed binaries (F4), which are host filesystem state.
- Across threads (F5): whether a thread reads its gid before or after another thread's credential change reaches it depends on the schedule, which Hermit makes repeat.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04). Detcore keeps a per-process credential record: real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets. `getegid` reports the record's effective gid. The record starts with every id 0, `fork` and `clone` copy it, and `execve` keeps it. `setgid`, `setregid` and `setresgid` update it and apply Linux's own permission rules against the record (`CAP_SETGID` from the record's capability sets, which a full uid drop clears); their tracking bodies describe the update side. The record is not enforced: the kernel goes on checking the guest's real credentials, root in its user namespace.

What the code does at hermit f4055eb9cae4, where no record exists yet:

- The typed dispatch arm returns `Ok(0)` for `getuid`, `geteuid`, `getgid` and `getegid` without reading guest state (`detcore/src/lib.rs:2760`-`2763`). Its comment (`detcore/src/lib.rs:2752`, tagged `TODO-HUMAN-REVIEW(#1549)`) says the answer is never forwarded to the host. The model replaces the constant with the record's effective gid and keeps the "never forwarded" property.
- `getegid` is classified `Determinized` at `detcore/src/syscall_classification.rs:672`. The comment at `detcore/src/syscall_classification.rs:657`-`668` explains the move away from pass-through: the query family read 0 only because the ptrace backend's `CLONE_NEWUSER` container maps the launching user to 0 (`.map_root()` at `hermit-cli/src/bin/hermit/run.rs:4631`), and an in-process backend leaked the host uid and gid.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`) asserts the classification (`detcore/src/syscall_classification.rs:2312`), not the value the arm returns.
- Nothing updates an identity today: the setters are no-ops returning 0 (`is_credential_identity_noop_syscall`, `detcore/src/syscall_classification.rs:974`), and `getresgid` writes 0 to each result pointer (`detcore/src/syscalls/misc.rs:702`).
- Reduced fidelity (owner decision 3): under `--no-namespace` (`--core-only`) or a backend without a user namespace, `getegid` still reads the Detcore value while the group of a new file and group permission checks follow the launching host user's real effective gid (the doc comment at `detcore/src/syscall_classification.rs:1002` records the same split for `stat` and `getuid`). No fidelity is required there.

Why `model` and not the alternatives:

- `emulate`, the constant 0 of today, makes a group drop invisible: after `setegid(1000)` the guest still reads 0.
- `passthrough` made the answer depend on the backend's namespace setup and on the launching user's group.
- `refuse` would break programs that check their own group, such as `groups` and `id`.
- The accepted cost of `model` is that the modelled effective gid is not enforced: new files keep the container's real group, and group permission checks use it.

### Differences from Linux

- **Today, credential changes are invisible (F3).** At hermit f4055eb9cae4, after `setegid(1000)` or `setgid(1000)`, accepted as no-ops, `getegid()` still returns 0. The model removes this difference.
- **No enforcement (target).** After a modelled change the kernel still uses the container's real effective gid for new files and permission checks.
- **Set-group-ID execs (F4).** Today `getegid()` stays 0 after `execve` of a set-group-ID binary. Decision 4 says `execve` keeps the record, so the bit stays unmodelled under the target and the effective gid read after the exec is the one from before it (a documented difference). In the default container only the launching user's primary group is mapped (described at `hermit-cli/tests/liteinst_advanced.rs:422`-`424`), so the kernel ignores the bit on a binary owned by any other host group as well.
- **Thread scope (F5, target).** The record is per process, so a raw `SYS_setresgid` in one thread changes what every thread reads; on Linux only the calling thread changes. The glibc wrappers change every thread on Linux too.
- **No overflow gid (F2).** Hermit reports the record where Linux would return 65534. `unshare` is refused with `EPERM` (`detcore/src/syscall_classification.rs:951`). `clone` and `clone3` with `CLONE_NEWUSER` do create the namespace today: `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`) injects the flags unchanged (`detcore/src/syscalls/threads.rs:924`; https://github.com/rrnewton/hermit/issues/3676), and the child still reads the record, because `getegid` is never forwarded.
- Target (owner decision 15, 2026-10-04): `clone` and `clone3` with any `CLONE_NEW*` flag return `EPERM`, as `unshare` already does, so a guest cannot enter a fresh user namespace and F2 cannot arise under Hermit. A program that tries to sandbox itself in a user namespace (a browser or container tool) gets the `EPERM` it already handles from unprivileged Linux.
- **`getgroups` is passed through today** (`detcore/src/syscall_classification.rs:718`). Under the default container the guest reads 0 plus 65534 for every other inherited supplementary group, a list that depends on the launching user. Under decision 4 it reads the record's group list instead, which starts empty.
- **`/proc/<pid>/status` is not covered.** Its `Gid:` line shows the kernel's credentials, so it disagrees with the record after a change. That stays an open item for `/proc` virtualization.

## Recommended tests

- T1 (covers F1): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_getegid)` returns 0, the record's starting value, in the first process, in a forked child and after an ordinary `execve`, identical across runs. Before any credential change, also compare it with the group of a newly created file and the `Gid:` line of `/proc/self/status`, and assert they agree.
  Existing partial coverage: the compat row `groups` (`/usr/bin/groups`, `tests/e2e/manifests/compat.yaml:154`) prints the guest's group names on ptrace; its comparator checks only that runs agree.
- T2 (covers F2): inside the guest, raw `clone(CLONE_NEWUSER | SIGCHLD)` and `clone3` with `CLONE_NEWUSER` both return `EPERM` (owner decision 15), identically across runs, and `getegid()` in the caller still reads the record. Natively the child would read 65534 before any `gid_map` write. Today the clone succeeds and the child reads the record, so the `EPERM` assertion fails until the clone-flag filter lands.
- T3 (covers F3): read-back through the record under `--strict` on ptrace. As root, `setresgid(-1, 1000, -1)` makes `getegid()` read 1000 and `setegid(0)` makes it read 0 again; `setfsgid(3000)` and `setgroups(1, {4000})` leave it at 0. `setgid(2000)` called as root sets all three gids, so `getegid()` reads 2000. After `setresuid(1000, 1000, 1000)` drops every uid, `setegid(0)` fails with `EPERM` and leaves it at 2000. A forked child reads the same value.
- T4 (covers F4): copy a helper binary into the guest's scratch directory, make it set-group-ID, `execve` it after `setresgid(-1, 1000, -1)`, and read `getegid()` in the new image; repeat with `prctl(PR_SET_NO_NEW_PRIVS, 1)`. Assert `getegid()` reads 1000 in both cases, because decision 4 keeps the record across `execve` (on Linux a mapped group's set-group-ID bit would change it unless `no_new_privs` is set; a documented difference), identically across runs.
- T5 (covers F5): in a process with two threads, one thread issues raw `syscall(SYS_setresgid, -1, 1000, -1)`. Then both threads read `getegid()`. Assert both read 1000, the per-process record's answer, identically across runs; the test lists the other thread's value as an accepted difference from Linux, where it stays 0.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (the dispatch arm returns the fixed virtual-root egid 0 and never forwards, matching the no-op setters); linux_determinism stays conditional (launching identity, user-namespace mapping and set-group-ID binaries); 5 flag behaviours and 5 tests listed; no test asserts the value, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, read-back of setegid/setgid/setresgid, Linux permission rules against the record, no enforcement); Hermit design rewritten with a reduced-fidelity line for --no-namespace and namespace-less backends; T1 narrowed to the strict ptrace container, T3 to T5 now assert read-back (temporary setegid drop and restore, EPERM after a full uid drop, per-process thread scope); no new F/T ids; both open questions removed (credential-family modelling and the passed-through getgroups list, answered by decision 4); T4 asserts execve keeps the record (set-group-ID bit unmodelled, a documented difference)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 15 applied (clone/clone3 with CLONE_NEWUSER get EPERM, so the overflow-id case F2 cannot arise; recorded that today the flags are injected unchanged at threads.rs:924; T2 now asserts EPERM); target_policy stays model; no new F/T ids; no open question removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getegid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getegid&body=Discussion+for+the+%60getegid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetegid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getegid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getegid`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The query family was moved from pass-through to the constant 0 under the marker `TODO-HUMAN-REVIEW(#1549)` (https://github.com/rrnewton/hermit/issues/1549); the setter no-ops under `TODO-HUMAN-REVIEW(#787)` (https://github.com/rrnewton/hermit/issues/787). No test file at hermit f4055eb9cae4 names `SYS_getegid`. `hermit-cli/tests/liteinst_advanced.rs:426` calls `getegid` on the host side to predict the guest's `groups` output; it is not a guest call.
