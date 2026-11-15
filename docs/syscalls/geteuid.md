---
schema: syscall-pages/v1
syscall: geteuid
nr_x86_64: 107
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+geteuid&body=Discussion+for+the+%60geteuid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgeteuid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60geteuid%60%2C+or+in+what+the+page+says%3F%0A
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

`geteuid()` returns the effective user ID of the calling thread, the uid the kernel uses for most permission checks. It takes no arguments and cannot fail. Hermit never forwards it: Detcore answers a constant 0 (`detcore/src/lib.rs:2760`-`2763` at hermit f4055eb9cae4), the fixed virtual-root identity shared with the other credential getters and the no-op setters. The target is a per-process credential record that the setters update (owner decision 4). See [geteuid(2)](https://man7.org/linux/man-pages/man2/geteuid.2.html).

## Linux semantics

The kernel returns `from_kuid_munged(current_user_ns(), current_euid())`: the effective uid in the calling thread's credentials, translated into the caller's user namespace. The call has no side effects and no error return.

The effective uid is inherited across `fork` and `clone`. It changes through `setuid` (both the privileged and the unprivileged form), `setreuid`, `setresuid`, and through `execve` of a set-user-ID binary. `setfsuid` changes only the filesystem uid, not the effective uid. Credentials belong to each thread: the raw setter syscalls change only the calling thread, and glibc's wrappers repeat the change on every thread.

### Flags and options

- F1: the return value is the effective uid of the calling thread's credentials, as seen from the caller's user namespace. The call never fails and has no side effects.
- F2: user-namespace translation: an effective uid with no mapping in the caller's namespace reads as the overflow uid (`/proc/sys/kernel/overflowuid`, 65534 by default), for example in a fresh `CLONE_NEWUSER` namespace before its `uid_map` is written.
- F3: credential changes: `setuid` (privileged or not), `setreuid` and `setresuid` change the effective uid; `setfsuid` does not; `fork` and `clone` copy it.
- F4: `execve` of a set-user-ID binary sets the effective uid to the file's owner. The bit is ignored when the mount is `nosuid`, when `no_new_privs` is set, when the owner has no mapping in the caller's user namespace, or when the process is traced by a tracer without the needed capability. Otherwise `execve` keeps the effective uid.
- F5: thread scope: a raw `SYS_setuid`/`SYS_setreuid`/`SYS_setresuid` changes only the calling thread, so two threads of one process can report different `geteuid()` values. The glibc wrappers apply the change to every thread.

## Determinism analysis

`linux_determinism: conditional`. The value is a pure function of the caller's credentials, its user-namespace mapping and, for F4, the owner and mount flags of each executed binary. Nothing in the call races. It repeats when every run starts with the same identity and mapping, executes the same files, and makes the same credential changes in the same order.

### Nondeterminism sources

- The identity that launched the program: natively the effective uid is whoever ran it.
- The user-namespace mapping (F2, and the mapping condition in F4).
- File ownership, set-user-ID bits and mount flags of executed binaries (F4), which are host filesystem state.
- Across threads (F5): whether a thread reads its uid before or after another thread's credential change reaches it depends on the schedule, which Hermit makes repeat.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04). Detcore keeps a per-process credential record: real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets. `geteuid` reports the record's effective uid. The record starts with every id 0, `fork` and `clone` copy it, and `execve` keeps it. `setuid`, `setreuid` and `setresuid` update it and apply Linux's own permission rules against the record, so a process that has dropped every uid to 1000 gets `EPERM` from `seteuid(0)`; their tracking bodies describe the update side. The record is not enforced: the kernel goes on checking the guest's real credentials, root in its user namespace.

What the code does at hermit f4055eb9cae4, where no record exists yet:

- The typed dispatch arm returns `Ok(0)` for `getuid`, `geteuid`, `getgid` and `getegid` without reading guest state (`detcore/src/lib.rs:2760`-`2763`). Its comment (`detcore/src/lib.rs:2752`, tagged `TODO-HUMAN-REVIEW(#1549)`) says the answer is never forwarded to the host. The model replaces the constant with the record's effective uid and keeps the "never forwarded" property.
- `geteuid` is classified `Determinized` at `detcore/src/syscall_classification.rs:670`. The comment at `detcore/src/syscall_classification.rs:657`-`668` explains why the query family stopped being passed through: it read 0 only because the ptrace backend's `CLONE_NEWUSER` container maps the launching user to 0 (`.map_root()` at `hermit-cli/src/bin/hermit/run.rs:4631`), and an in-process backend leaked the host uid.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`) asserts the classification (`detcore/src/syscall_classification.rs:2310`), not the value the arm returns.
- Nothing updates an identity today: the setters are no-ops returning 0 (`is_credential_identity_noop_syscall`, `detcore/src/syscall_classification.rs:974`), and `getresuid` writes 0 for the effective uid too (`detcore/src/syscalls/misc.rs:687`-`688`).
- Reduced fidelity (owner decision 3): under `--no-namespace` (`--core-only`) or a backend without a user namespace, `geteuid` still reads the Detcore value while the kernel checks permissions with the launching host user's effective uid, so a guest that reads `geteuid() == 0` can still get `EACCES` from a root-only file, and `stat` reports the real host uid (doc comment at `detcore/src/syscall_classification.rs:1002`). No fidelity is required there.

Why `model` and not the alternatives:

- `emulate`, the constant 0 of today, makes a privilege drop invisible: after `seteuid(1000)` or `setuid(1000)` the guest still reads 0, and programs that verify the drop see it fail.
- `passthrough` made the answer depend on the backend's namespace setup and on who launched Hermit.
- `refuse` would break every program that checks whether it runs as root.
- The accepted cost of `model` is that the modelled effective uid is not enforced: after `seteuid(1000)` the guest can still open root-only files.

### Differences from Linux

- **Today, credential changes are invisible (F3).** At hermit f4055eb9cae4, after `seteuid(1000)` or `setuid(1000)`, accepted as no-ops, `geteuid()` still returns 0. The model removes this difference.
- **No enforcement (target).** After a modelled drop the kernel still applies root's permissions inside the container.
- **Set-user-ID execs (F4).** Today `geteuid()` stays 0 after `execve` of a set-user-ID binary. Decision 4 says `execve` keeps the record, so the bit stays unmodelled under the target and the effective uid read after the exec is the one from before it (a documented difference). In the default container this rarely differs in practice, because the only mapped uid is the launching user, so the bit on a binary owned by any other host user is ignored by the kernel as well.
- **Thread scope (F5, target).** The record is per process, so a raw `SYS_setresuid` in one thread changes what every thread reads; on Linux only the calling thread changes. The glibc wrappers change every thread on Linux too.
- **No overflow uid (F2).** Hermit reports the record where Linux would return 65534. `unshare` is refused with `EPERM` (`detcore/src/syscall_classification.rs:951`). `clone` and `clone3` with `CLONE_NEWUSER` do create the namespace today: `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`) injects the flags unchanged (`detcore/src/syscalls/threads.rs:924`; https://github.com/rrnewton/hermit/issues/3676), and the child still reads the record, because `geteuid` is never forwarded.
- Target (owner decision 15, 2026-10-04): `clone` and `clone3` with any `CLONE_NEW*` flag return `EPERM`, as `unshare` already does, so a guest cannot enter a fresh user namespace and F2 cannot arise under Hermit. A program that tries to sandbox itself in a user namespace (a browser or container tool) gets the `EPERM` it already handles from unprivileged Linux.
- **`/proc/<pid>/status` is not covered.** Its `Uid:` line shows the kernel's credentials, so it disagrees with the record after a change. That stays an open item for `/proc` virtualization.

## Recommended tests

- T1 (covers F1): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_geteuid)` returns 0, the record's starting value, in the first process, in a forked child and after an ordinary `execve`, identical across runs.
  Existing partial coverage: the compat corpus rows `id` (`/usr/bin/id -u`, `tests/e2e/manifests/compat.yaml:71`) and `whoami` (`tests/e2e/manifests/compat.yaml:153`) print the effective uid on ptrace. Their comparator checks only that runs agree, not that the value is 0.
- T2 (covers F2): inside the guest, raw `clone(CLONE_NEWUSER | SIGCHLD)` and `clone3` with `CLONE_NEWUSER` both return `EPERM` (owner decision 15), identically across runs, and `geteuid()` in the caller still reads the record. Natively the child would read 65534 before any `uid_map` write. Today the clone succeeds and the child reads the record, so the `EPERM` assertion fails until the clone-flag filter lands.
- T3 (covers F3): read-back through the record under `--strict` on ptrace. As root, `setresuid(-1, 1000, -1)` makes `geteuid()` read 1000, and `seteuid(0)` (allowed, 0 is still the real and saved uid) makes it read 0 again. `setfsuid(3000)` leaves it at 0. `setuid(1000)` called as root sets all three uids, so `geteuid()` reads 1000, and a following `seteuid(0)` fails with `EPERM` and leaves it at 1000. A forked child reads the same value.
- T4 (covers F4): copy a helper binary into the guest's scratch directory, make it set-user-ID, `execve` it after `setresuid(-1, 1000, -1)`, and read `geteuid()` in the new image; also try it with `prctl(PR_SET_NO_NEW_PRIVS, 1)`. Assert `geteuid()` reads 1000 in both cases, because decision 4 keeps the record across `execve` (on Linux a mapped owner's set-user-ID bit would change it unless `no_new_privs` is set; a documented difference), identically across runs.
- T5 (covers F5): in a process with two threads, one thread issues raw `syscall(SYS_setresuid, -1, 1000, -1)`. Then both threads read `geteuid()`. Assert both read 1000, the per-process record's answer, identically across runs; the test lists the other thread's value as an accepted difference from Linux, where it stays 0.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (the dispatch arm returns the fixed virtual-root euid 0 and never forwards, matching the no-op setters); linux_determinism stays conditional (launching identity, user-namespace mapping and set-user-ID binaries); 5 flag behaviours and 5 tests listed; no test asserts the value, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, read-back of seteuid/setuid/setresuid, Linux permission rules against the record, no enforcement); Hermit design rewritten with a reduced-fidelity line for --no-namespace and namespace-less backends; T1 narrowed to the strict ptrace container, T3 to T5 now assert read-back (temporary seteuid drop and restore, EPERM after a full drop, per-process thread scope); no new F/T ids; open questions on credential-family modelling (decision 4) and --no-namespace fidelity (decision 3) removed; T4 asserts execve keeps the record (set-user-ID bit unmodelled, a documented difference)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 15 applied (clone/clone3 with CLONE_NEWUSER get EPERM, so the overflow-id case F2 cannot arise; recorded that today the flags are injected unchanged at threads.rs:924; T2 now asserts EPERM); target_policy stays model; no new F/T ids; no open question removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `geteuid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+geteuid&body=Discussion+for+the+%60geteuid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgeteuid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60geteuid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `geteuid`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The query family was moved from pass-through to the constant 0 under the marker `TODO-HUMAN-REVIEW(#1549)` (https://github.com/rrnewton/hermit/issues/1549); the setter no-ops under `TODO-HUMAN-REVIEW(#787)` (https://github.com/rrnewton/hermit/issues/787). No test file at hermit f4055eb9cae4 names `SYS_geteuid`.
