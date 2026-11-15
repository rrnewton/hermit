---
schema: syscall-pages/v1
syscall: symlink
nr_x86_64: 88
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/487
  - https://github.com/rrnewton/hermit/issues/489
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+symlink&body=Discussion+for+the+%60symlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsymlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60symlink%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 2
---

## Summary

`symlink(target, linkpath)` creates a symbolic link at `linkpath` whose contents are the string `target`, stored verbatim and never resolved. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no link state of its own.

## Linux semantics

The kernel resolves the parent of the last component of `linkpath`, following symlinks in every earlier component, and creates a new inode of type `S_IFLNK` there. `target` is copied byte for byte. It is not looked up, so it may name a missing path (a dangling link), and a relative `target` is interpreted only later, relative to the directory that holds the link, each time the link is followed. `lstat` reports the new link's `st_size` as the length of `target`. On success the kernel sets the new inode's timestamps to the current time and updates the parent directory's mtime and ctime. The call returns 0 or a negative errno. It never reports the inode number or the timestamps; a guest sees those only through the stat family and `getdents`. Errors are checked in path-walk order, so the errno is a function of the two strings, the filesystem state, the credentials and the mount flags.

Manual reference: [symlink(2)](https://man7.org/linux/man-pages/man2/symlink.2.html).

### Flags and options

- F1: `target` contents: stored verbatim and not resolved. An absolute, relative or missing `target` all succeed, and `readlink` returns the exact bytes. An empty `target` gives `ENOENT`. A `target` longer than the filesystem's limit (a page, minus one byte, on most filesystems) gives `ENAMETOOLONG`.
- F2: `linkpath` already exists as any type, including another symlink, dangling or not: `EEXIST`. The existing entry is never followed or replaced.
- F3: trailing slash on `linkpath`: when the name does not exist, `symlink("t", "new/")` fails with `ENOENT`, because a trailing slash demands a directory. When it exists, the result is `EEXIST`.
- F4: mode and ownership: the link's mode is always `0777`; the umask and `mode` bits do not apply. The owner is the caller's filesystem uid. The group is the caller's filesystem gid, unless the parent is setgid (or the filesystem is mounted `grpid`), in which case the link takes the parent's group.
- F5: an intermediate component of `linkpath` is missing, or `linkpath` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`. Too many symlinks in the prefix: `ELOOP`. A component longer than `NAME_MAX`, or a path longer than `PATH_MAX`: `ENAMETOOLONG`.
- F6: search or write permission is denied on the parent path: `EACCES`. F11 holds `EPERM` from a filesystem without symlink support.
- F7: `target` or `linkpath` points outside the address space: `EFAULT`
- F8: `EROFS` (the parent is on a read-only mount), and `ENOSPC` from a container-owned filesystem of fixed size. F10 holds the host's own space, quota and I/O failures.
- F9: a relative `linkpath` resolves against the calling process's working directory, inside its root and mount namespace
- F10: host-transient failures: `ENOSPC` (the host filesystem has no space or no inodes left), `EDQUOT` (a host quota is exceeded) and `EIO` (failing storage).
- F11: the parent's filesystem does not support symlinks: `EPERM`.

## Determinism analysis

The result is a function of the two argument strings, the filesystem state along `linkpath`, the caller's credentials and the mount flags. The new link's contents are exactly `target`, so they are input-determined. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so two guest threads racing to create the same `linkpath` get a fixed order and a fixed winner. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it. That condition is why the classification is `conditional` rather than `deterministic`: Hermit holds the filesystem stable by assumption, not by modelling it.

### Nondeterminism sources

- Changes to the filesystem made outside the container: path existence, directory permissions, mount behaviour and security policy all decide the errno. Hermit does not try to make a changing host filesystem deterministic.
- Host resources behind F8, F10 and F11: free space, inode count, quota, read-only remounts, I/O errors and the filesystem type.
- Values the call creates but does not return: the inode number and timestamps. They become visible only through `lstat`, `getdents` and similar calls, and are handled there.

### Cross-host variation

- Filesystem type behind the guest's paths: the longest `target` it stores before `ENAMETOOLONG` (F1), and whether it supports symlinks at all (`EPERM`, F11).
- Mount options of the host filesystem: under `grpid` the link takes the parent's group (F4).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `symlink` is classified `PassThrough` at `detcore/src/syscall_classification.rs:799`, inside the group whose comment at `detcore/src/syscall_classification.rs:767` says that "Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image", with the open review marker `TODO-HUMAN-REVIEW(#683)` at `detcore/src/syscall_classification.rs:769`. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `Sysno::symlink` as `PassThrough` at `detcore/src/syscall_classification.rs:2086`. Its `*at` sibling `symlinkat` is passed through by the same table at `detcore/src/syscall_classification.rs:734`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F9 the same way the guest's later `readlink`, `lstat`, `open` and path walks see them.
- The call returns only 0 or an errno. The two values it creates and does not return, the inode number and the timestamps, are the stat family's concern.
- Emulating the call would mean answering path lookups without the kernel, which is the job of a filesystem model Hermit does not have.

Code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `symlink` out. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree, so this body does not describe how record captures `symlink`. The record/replay integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires it to replay from the recording: its guest issues raw `syscall(SYS_symlink, "b", "d")` at `tests/c/record_replay_path_queries.c:240` and checks the result, and the harness at `hermit-cli/tests/record_replay.rs:562` runs it with `--backend=ptrace`.

The open gap is run-mode test evidence, not code. The only test that names `SYS_symlink` is that record/replay workload. The run-mode verify cells call glibc `symlink()`: `tests/c/symlink_ops.c:129` (a dangling link, cell `c-programs/symlink-ops` at `tests/e2e/manifests/c-programs.yaml:13904`) and `tests/c/syscall_file_metadata.c:160` (cell `c-programs/syscall-file-metadata`, `tests/e2e/manifests/c-programs.yaml:9275`). On the host checked (glibc 2.34, x86_64) the glibc wrapper issues `SYS_symlink`, but a test that pins this tracking issue should call `syscall(SYS_symlink, ...)` so that a libc switching to `symlinkat` cannot quietly remove the coverage.

## Recommended tests

- T1 (covers F1, F9): under `hermit run --verify` on ptrace, `chdir` into a fresh directory and call raw `syscall(SYS_symlink, ...)` with relative `linkpath` values and with an absolute, a relative, a missing and a 4095-byte `target`. Assert `readlink` returns the exact bytes and `lstat` reports `S_IFLNK` with `st_size == strlen(target)`. Assert `ENOENT` for an empty `target` and `ENAMETOOLONG` for a `target` of 4096 bytes or more.
  Existing partial coverage via glibc `symlink()` and `symlinkat()`: `tests/c/symlink_ops.c:89` and `tests/c/symlink_ops.c:129` (cell `c-programs/symlink-ops`).
- T2 (covers F2, F3): assert `EEXIST` when `linkpath` is an existing directory, a regular file, a live symlink and a dangling symlink, and that the existing entry is unchanged afterwards. Assert `ENOENT` for `symlink("t", "new/")` when `new` does not exist.
- T3 (covers F4): under umask `077`, assert the new link's `lstat` mode is `S_IFLNK | 0777`. Make the parent setgid with a second mapped group and assert the link's `st_gid` is the parent's group; under a non-setgid parent assert it is the caller's gid.
- T4 (covers F5, F7): assert `ENOENT` for a missing intermediate component and for `""`, `ENOTDIR` with a regular file as an intermediate component, `ELOOP` for a symlink loop in the prefix, `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, and `EFAULT` for `target = (char *)1` and for `linkpath = (char *)1`.
- T5 (covers F6): assert `EACCES` when creating under a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. Record the chosen paths in the test. `EPERM` from a filesystem without symlink support is F11, recorded by U2.
- T6 (covers F8): assert `EROFS` under a read-only mount the guest can see, and `ENOSPC` on a small size-limited tmpfs. This `ENOSPC` comes from a container-owned filesystem of fixed size, so it is guest-determined. The host's own `ENOSPC`, `EDQUOT` and `EIO` are F10, recorded by U1.
- U1 (covers F10; host-transient): `ENOSPC`, `EDQUOT` and `EIO` depend on the host filesystem's free space, quotas and storage health, shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F10 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F11; host-config): which filesystems lack symlink support, and whether one is visible in the guest, is set by the host, so CI cannot rely on producing this error from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:799`, `detcore/src/lib.rs:2843`).
- T7 (covers F1, F2): record/replay: `hermit record` with `--verify` replays a raw `SYS_symlink` success and an `EEXIST` failure from the recording. Existing coverage for the success case: `tests/c/record_replay_path_queries.c:240`, driven by `hermit-cli/tests/record_replay.rs:2171`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/487: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/489 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843, classified at detcore/src/syscall_classification.rs:799; Hermit owns no link state); merged duplicate https://github.com/rrnewton/hermit/issues/489 (its DETERMINISTIC lost to conditional, see Additional information); listed 9 flag behaviours and 7 tests; no coverage token recorded because no run-mode ptrace cell calls SYS_symlink directly, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type for the target length limit and symlink support, grpid mount option); F6 narrowed to EACCES, new F11 EPERM without symlink support (host-config U2); F8 narrowed to EROFS and container-owned ENOSPC, new F10 host ENOSPC/EDQUOT/EIO (host-transient U1); the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `symlink`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+symlink&body=Discussion+for+the+%60symlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsymlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60symlink%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: symlink" (https://github.com/rrnewton/hermit/issues/487), legacy variant A1.

Duplicate https://github.com/rrnewton/hermit/issues/489 was filed 2026-07-24 as "Syscall classification: symlink" (legacy variant A1) and is merged into this body. Its comment history (0 comments) stays on that issue; the comment history of https://github.com/rrnewton/hermit/issues/487 stays on that issue.

Merge reconciliation:

- Classification: https://github.com/rrnewton/hermit/issues/487 said CONDITIONAL; https://github.com/rrnewton/hermit/issues/489 said DETERMINISTIC ("deterministic given fixed FS state"). Both describe the same fact: the result repeats given a fixed filesystem state. `conditional` is kept because that fixed state is an assumption Hermit does not enforce, and the errno changes when a process outside the container mutates the filesystem.
- Nondeterminism sources: https://github.com/rrnewton/hermit/issues/489 named only external concurrent filesystem mutation; https://github.com/rrnewton/hermit/issues/487 also named directory permissions, mount behaviour and security policy. Both lists are kept under Determinism analysis.
- Kernel note from https://github.com/rrnewton/hermit/issues/489 ("the target is stored verbatim, not resolved") is F1. Both issues recommended PASS-THRU, which is the target policy.

### Open questions

- New inode numbers and timestamps are outside this issue. They belong to the stat family and getdents tracking.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=symlink`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `symlink` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
symlink("/nonexistent", "/tmp/hermit-b5-link-3143095") = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence from https://github.com/rrnewton/hermit/issues/489

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=symlink` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `symlink` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`. Verified: creating a /tmp symlink returned 0.

Observed strace line:
```
symlink("/etc/hostname", "/tmp/sysclass45/sl") = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-10-03 glibc wrapper check

On the measurement host (glibc 2.34, x86_64), `strace -e trace=symlink,symlinkat` of a program calling glibc `symlink()` showed `symlink("/nonexistent", ".../l") = 0`: the wrapper issues `SYS_symlink`, not `symlinkat`.
