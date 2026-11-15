---
schema: syscall-pages/v1
syscall: chroot
nr_x86_64: 161
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/338
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chroot&body=Discussion+for+the+%60chroot%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchroot.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chroot%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`chroot(path)` changes the calling process's root directory, the directory that `/` and leading-`/` paths resolve from. Hermit forwards it to the host kernel unchanged. Under Hermit's default configuration the guest runs in a user namespace of its own (`hermit-cli/tests/chown_virtual_root_identity.rs:182`), whose creator holds `CAP_SYS_CHROOT`, so the call succeeds whenever the target directory exists and is searchable. The result depends only on the guest's filesystem view and its credentials.

## Linux semantics

The kernel resolves `path` with symlinks followed, from the caller's current root and working directory. It requires search (execute) permission on the target, requires it to be a directory, and requires `CAP_SYS_CHROOT` in the caller's user namespace. It then replaces the root in the caller's `fs_struct`. The working directory does not change, so a process can stay outside its new root until it calls `chdir`. Threads created with `CLONE_FS` share one `fs_struct` and change root together. Other threads and processes keep theirs. `fork` and `execve` inherit the new root. Mounts are not affected: `chroot` is a per-process view, not a mount-namespace change.

### Flags and options

- F1: success: the root becomes `path` and the call returns 0. Later absolute paths, and `..` at the new root, resolve inside it, and `/proc/self/root` names it. The working directory is unchanged.
- F2: scope: threads sharing the `fs_struct` (`CLONE_FS`, as with `pthread_create`) see the new root. A process created afterwards by `fork` or `clone` without `CLONE_FS` inherits a copy, and `execve` keeps it.
- F3: relative `path`: resolved from the current working directory. Symlinks in `path` are followed, including a final component that is a symlink to a directory.
- F4: no `CAP_SYS_CHROOT` in the caller's user namespace: `EPERM`
- F5: search permission denied on a component of `path`, or on the target directory itself: `EACCES`
- F6: a component does not exist, or `path` is the empty string: `ENOENT`. A component, or the target, is not a directory: `ENOTDIR`.
- F7: `path` or a component too long: `ENAMETOOLONG`. Too many symlinks: `ELOOP`.
- F8: `path` cannot be read: `EFAULT`. An I/O error or no kernel memory: `EIO`, `ENOMEM`.
- F9: after a successful `chroot`, the process may no longer create a user namespace: `unshare(CLONE_NEWUSER)` and `clone(CLONE_NEWUSER)` fail with `EPERM` while the root differs from the mount namespace's root. Under Hermit both get `EPERM` with or without a `chroot` (owner decision 15), so the guest sees the same answer.

## Determinism analysis

The result is a function of the path bytes, the guest's root and working directory, the directory tree under it, and the guest's credentials. Hermit's model of a fixed container and stable filesystem makes the first three repeatable. The credentials are fixed by the Hermit configuration. No timing or other process enters the result, and Hermit runs threads one at a time, so an `F2` change of root races with nothing. Linux behaviour is therefore conditional: deterministic as long as the directory tree the path names does not change between runs.

### Nondeterminism sources

- External mount or path mutation: a directory created, removed or re-permissioned outside the guest between runs changes F1, F5 or F6
- Credential and capability policy: F4 depends on whether the guest has `CAP_SYS_CHROOT`. That is fixed per Hermit configuration (see Hermit design), not per run.

## Hermit design

Target policy: `passthrough`. The code already does this.

- `chroot` is classified `PassThrough` at `detcore/src/syscall_classification.rs:812` (hermit f4055eb9cae4), the first entry of the issue-reviewed pass-through block at `detcore/src/syscall_classification.rs:807`. That block's comment (`detcore/src/syscall_classification.rs:808`-`809`) says these calls stay "conditional on Hermit's fixed-container and stable-state assumptions".
  The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the class at `detcore/src/syscall_classification.rs:2101`. The census list holds it at `detcore/src/syscall_classification.rs:1686`.
- Interception charges the fixed `_ => FAST_NS` cost (250 ns) at `detcore/src/syscall_time.rs:156`, through `detcore/src/lib.rs:1936`. Code facts show `subscribed_record_replay: false`.

Why passthrough, not `model` or `emulate`: the kernel is what resolves every later path from the new root, so Detcore cannot change the root without the kernel doing it. Detcore's own path handling already follows a guest `chroot`. To turn an `AT_FDCWD`-relative open into a guest-absolute path, `resolved_at_fdcwd_path` reads `/proc/<pid>/root` and `/proc/<pid>/cwd` and strips the root prefix (`detcore/src/syscalls/files.rs:437`-`449`). `/proc/<pid>/root` reflects the guest's current root, so a guest `chroot` changes that prefix the same way in every run.

Target (owner decision 15, 2026-10-04), for F9 only: every namespace request gets `EPERM`. `unshare` and `setns` already do (`detcore/src/syscall_classification.rs:951`-`952`, dispatched at `detcore/src/lib.rs:2216`), so `unshare(CLONE_NEWUSER)` fails after a `chroot` as on Linux, and before one too. `clone` with `CLONE_NEWUSER` is injected unchanged today (`detcore/src/syscalls/threads.rs:924`), so after a `chroot` the kernel's own rule gives `EPERM`; at the target Detcore gives it first.

Interactions this body records but does not resolve:

- Detcore classifies some opens by the guest's path spelling, for example `/dev/random` and `/dev/urandom` as the deterministic random-number source at `detcore/src/syscalls/files.rs:1014`. After a guest `chroot`, that spelling names whatever the new root holds at that path. This is the same exposure a symlink or bind mount already creates, and `chroot` adds no new way past it.
- The record/replay replayer runs the guest in a chroot of its own whose `/proc` is absent (`detcore/src/syscalls/files.rs:439`, `486`-`488`). Code facts show record/replay does not subscribe `chroot`. This body does not establish how a guest `chroot` behaves in replay, where the forwarded call resolves inside the replay root.

Existing evidence: `hermit-cli/tests/procfs_determinism.rs:1208` (`chroot_mountinfo_subset_keeps_fdinfo_identity_consistent`) runs a guest inside a chroot, but the launcher applies that chroot before `exec` (`command.chroot(root.path())` at `hermit-cli/tests/procfs_determinism.rs:1323`). The guest never calls `SYS_chroot`, so it does not cover this syscall.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) the guest has no user namespace of its own and, unless Hermit itself runs as host root, no `CAP_SYS_CHROOT`, so `chroot` returns `EPERM` (F4) while Hermit reports `getuid() == 0`; in-process backends have no namespace at all (`detcore/src/syscall_classification.rs:1007`-`1008`) and behave the same. The result is fixed per configuration, so it does not break determinism.

## Recommended tests

- T1 (covers F1, F3): under `hermit run --verify` on ptrace in the default configuration, create `jail/` under the guest's working directory with a marker file `jail/marker`. Call `chroot("jail")` (relative) and assert 0. Assert that `getcwd` still reports the old directory and that `open("/marker")` succeeds.
  Then `chdir("/")`, assert that `open("../marker")` also succeeds (`..` stays at the root), and that `readlink("/proc/self/root")` is consistent across runs if `/proc` is mounted in the jail.
  Repeat through a symlink `link -> jail`.
- T2 (covers F2): after `chroot`, a second `pthread` thread opens `/marker` and succeeds. A `fork` child does the same, then `execve`s a static helper inside the jail that does it too. A thread created with `clone` without `CLONE_FS` before the `chroot` still sees the old root.
- T3 (covers F4): under `hermit run --strict` on ptrace in its container, drop `CAP_SYS_CHROOT` from the effective set with `capset` (forwarded today, `detcore/src/syscall_classification.rs:717`), then assert that `chroot("jail")` returns `EPERM` and that the root is unchanged. Once `capset` is modelled without enforcement (owner decision 4) it no longer drops the kernel capability, and `prctl(PR_CAPBSET_DROP)` returns `ENOSYS` on ptrace (`detcore/src/syscalls/misc.rs:395`, `:403`), so the test must then name another route or state that F4 has none in the strict container.
- T4 (covers F5, F6, F7): assert `EACCES` for a directory with mode `0600` after the guest drops `CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` with `capset` while keeping `CAP_SYS_CHROOT`. The default `uid_map` maps one id, so `setuid` to another uid is not available. Assert `ENOENT` for a missing path and for `""`, `ENOTDIR` for a regular file and for `file/x`, `ENAMETOOLONG` for a 4096-byte path, and `ELOOP` for a symlink cycle.
- T5 (covers F8): assert `EFAULT` for `chroot((char *)1)`. `EIO` and `ENOMEM` cannot be provoked deterministically and are listed for completeness.
- T6 (covers F9): after `chroot("jail")` in the default configuration, assert that `unshare(CLONE_NEWUSER)` and a raw `clone(CLONE_NEWUSER | SIGCHLD)` both return `EPERM` and create no child

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/338#issuecomment-5872947089
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/338: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; chroot records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/338 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (result depends on the guest's directory tree and fixed credentials; forwarded at detcore/src/lib.rs:2843; Detcore's cwd resolution follows a guest chroot via /proc/<pid>/root, detcore/src/syscalls/files.rs:437-449); listed 9 behaviours and 6 tests; no test calls SYS_chroot (procfs_determinism.rs chroots in the launcher), so tracking_status stays todo; recorded the --no-namespace EPERM fidelity gap and unverified replay behaviour
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): the --no-namespace EPERM interaction became the reduced-fidelity line (no CAP_SYS_CHROOT under --no-namespace and in-process backends); target_policy unchanged (passthrough); T3 narrowed from --no-namespace to the strict ptrace container, dropping CAP_SYS_CHROOT with capset, with a note that a decision-4 capset model removes that route; no new F/T ids; no open questions existed
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 15 applied to F9 only (namespace requests always get EPERM, so the post-chroot user-namespace refusal holds with or without chroot; T6 now also asserts clone(CLONE_NEWUSER)); target_policy stays passthrough (the call is forwarded, not refused, so decision 6 does not apply); no new F/T ids; no open question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `chroot`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chroot&body=Discussion+for+the+%60chroot%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchroot.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chroot%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: chroot" (https://github.com/rrnewton/hermit/issues/338), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/338.

The legacy summary added two lines that this body moves here: "Rank 59 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls." and "Manual reference: Linux man-pages chroot(2)."

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=chroot -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(161, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
chroot(0xffffffffffffffff)      = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/338#issuecomment-5872947089
