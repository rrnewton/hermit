---
schema: syscall-pages/v1
syscall: unlinkat
nr_x86_64: 263
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/506
  - https://github.com/rrnewton/hermit/issues/527
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+unlinkat&body=Discussion+for+the+%60unlinkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funlinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60unlinkat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`unlinkat(dirfd, pathname, flags)` removes a directory entry, resolved against `dirfd` or the working directory. With `flags` 0 it behaves like `unlink`; with `AT_REMOVEDIR` it removes an empty directory, like `rmdir`. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no filesystem state of its own.

## Linux semantics

The kernel accepts only `AT_REMOVEDIR` in `flags`; any other bit gives `EINVAL`. It looks up the parent of the last component, starting from `dirfd` for a relative path and ignoring it for an absolute one, and never follows the last component: a symlink is removed as a link and its target is untouched. With `flags` 0 the entry must not be a directory. Removing a name decrements the inode's link count; the inode and its data are freed only when the count reaches zero and no process still has the file open or mapped, so a file can be unlinked while open and remain readable through the descriptor. With `AT_REMOVEDIR` the directory must be empty, and `.` and `..` as the last component are refused. On success the parent's mtime and ctime change, and the removed inode's ctime changes if it survives. The call returns 0 or a negative errno and reports nothing else. On the host, coreutils `rm` issues `unlinkat(AT_FDCWD, path, 0)` (strace evidence below), so most shell-driven file removal reaches this syscall.

### Flags and options

- F1: `flags == 0` on a non-directory (regular file, FIFO, socket, device, symlink): removes the name and decrements the link count. A hard-linked file survives under its other names.
- F2: `flags == 0` on a directory: `EISDIR`
- F3: `AT_REMOVEDIR` on an empty directory: removes it. A trailing slash on the last component is accepted. Host check: `"m2/"` was removed.
- F4: `AT_REMOVEDIR` on a non-directory: `ENOTDIR`
- F5: `AT_REMOVEDIR` on a non-empty directory: `ENOTEMPTY` (POSIX also allows `EEXIST`; Linux filesystems return `ENOTEMPTY`)
- F6: `AT_REMOVEDIR` with `.` as the last component: `EINVAL`. With `..` as the last component: `ENOTEMPTY`.
- F7: any other bit in `flags`, including `AT_SYMLINK_NOFOLLOW` (0x100), `AT_EMPTY_PATH` (0x1000) and bit 0: `EINVAL`
- F8: the last component is a symlink, whether to a file or a directory, with `flags` 0: the link is removed and its target is unchanged
- F9: `flags == 0` with a trailing slash on a non-directory name: `ENOTDIR`
- F10: the entry is still open in this or another guest process: the name is removed, and the inode stays readable and writable through the open descriptor until the last close
- F11: the parent directory is sticky (`S_ISVTX`) and the caller owns neither the parent nor the entry and lacks `CAP_FOWNER`: `EPERM`. F22 holds immutable and append-only entries.
- F12: the target is a mount point, or is in use by the system in a way the filesystem refuses: `EBUSY`
- F13: the name does not exist, or `pathname` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F14: write or search permission is denied on the path: `EACCES`
- F15: a component is longer than `NAME_MAX`, or the path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in the prefix: `ELOOP`.
- F16: `pathname` points outside the address space: `EFAULT`
- F17: the entry is on a read-only mount: `EROFS`
- F18: `dirfd == AT_FDCWD`: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace
- F19: `dirfd` is an open directory, including an `O_PATH` one: a relative `pathname` resolves against it
- F20: `dirfd` is open but not a directory, with a relative `pathname`: `ENOTDIR`. `dirfd` is not open: `EBADF`.
- F21: an absolute `pathname` ignores `dirfd`
- F22: the entry or its parent carries the immutable or append-only attribute, which only the host can set: `EPERM`.

## Determinism analysis

The result is a function of the directory `dirfd` names, the filesystem state along the path, `flags`, the caller's credentials and capabilities, and the mount table. Nothing in the call depends on timing, and it does not block. Under Hermit, threads run one at a time, so two guest threads racing to remove or recreate the same name get a fixed order. F10 depends on which guest processes still hold the file open, which is itself guest state that Hermit's scheduler fixes. The call repeats exactly when the guest-visible filesystem starts in the same state, the guest runs with the same credentials, and nothing outside the container changes it.

`linux_determinism` stays `deterministic`. The merged duplicate said CONDITIONAL because results depend on external filesystem state (see the reconciliation under Additional information). That dependency is real, but it is the stable-state assumption Detcore states for this whole group at `detcore/src/syscall_classification.rs:725` ("repeat given stable guest-visible filesystem state with no external mutation"), and the siblings `unlink`, `rmdir`, `mkdir` and `mkdirat` carry `deterministic` under the same assumption. Unlike `fremovexattr`, no flag here has a result that changes with host kernel or LSM configuration while the container's files stay the same.

### Nondeterminism sources

- Changes to the filesystem made outside the container: the entry appearing, disappearing or changing type between guest calls. Hermit does not try to make a changing host filesystem deterministic.
- The mount table behind F12 and F17, which Hermit fixes for the container but which the host can change.
- The merged duplicate also listed directory-descriptor identity, flags, permissions and path state. Those are inputs the guest controls, not nondeterminism sources: given the same guest state they give the same result.

### Cross-host variation

- Host mount layout visible in the guest: which entries are mount points and give `EBUSY` (F12).
- Inode attributes set on the host: immutable or append-only entries give `EPERM` (F22).

## Hermit design

Target policy: `passthrough`. The code already does this. `unlinkat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:736`, next to `rmdir` (line 733) and `unlink` (line 735), inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The group comment at `detcore/src/syscall_classification.rs:725` gives the condition quoted above. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only delegates to the record/replay helper, so in run mode the host kernel executes the call as issued. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`; `unlinkat` at line 2092) pins the classification, not guest behaviour.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. Directory entries live in the guest's container mount namespace, and only the kernel can apply F1 to F21 consistently with what `stat`, `open`, `getdents` and the deferred-free rule of F10 later see.
- The call creates no kernel object and does not block, so there is no scheduling or time interaction for Detcore to control.
- Every sibling that changes the namespace (`unlink`, `rmdir`, `mkdir`, `mkdirat`, `linkat`, `renameat2`, `symlinkat`) is passed through on the same assumption. Handling `unlinkat` differently would split one filesystem view across two mechanisms.

Code facts show `subscribed_record_replay: false`: Detcore's `--passthru-opt` subscription leaves `unlinkat` out, so in that mode the call never reaches Detcore. `hermit record` has its own subscriptions in a recorder module that is not in the exported tree (comment at `detcore/src/lib.rs:1078`), so this body does not describe how record captures it.

The open gap is test evidence, not code. `tests_naming` is 0: no test names `SYS_unlinkat`. Existing run-mode coverage goes through glibc in `tests/c/linkat_flags.c`: removing a hard link with `flags` 0 and checking the link count returns to 1 (line 50), removing an empty directory with `AT_REMOVEDIR` and checking `lstat` gives `ENOENT` (line 53), and `ENOENT` for a missing name (line 58). That fixture runs in the ptrace verify cell `c-programs/linkat-flags` (`tests/e2e/manifests/c-programs.yaml:12263`) and exits nonzero unless all five of its checks pass (`tests/c/linkat_flags.c:74`).

## Recommended tests

- T1 (covers F1, F2, F3, F18, F19, F21): under `hermit run --verify` on ptrace, call raw `syscall(SYS_unlinkat, ...)` with `AT_FDCWD` after a `chdir`, with an `O_RDONLY|O_DIRECTORY` descriptor, with an `O_PATH` descriptor, and with an absolute path and a closed descriptor number. Remove a regular file, a hard-linked file (assert the other name's `st_nlink`), a FIFO and a socket node with `flags` 0; remove an empty directory with `AT_REMOVEDIR`, once with a trailing slash; assert `EISDIR` for a directory with `flags` 0.
  Existing partial coverage via glibc: `tests/c/linkat_flags.c:50` and `:53`.
- T2 (covers F4, F5, F6, F7): assert `ENOTDIR` for `AT_REMOVEDIR` on a regular file, `ENOTEMPTY` on a non-empty directory, `EINVAL` for `"."` and `ENOTEMPTY` for `"sub/.."` with `AT_REMOVEDIR`, and `EINVAL` for `flags` 0x1, 0x100 and 0x1000.
- T3 (covers F8, F9): create a symlink to a file and one to a directory, remove each with `flags` 0, and assert the targets still exist. Assert `ENOTDIR` for `"file/"` with `flags` 0.
- T4 (covers F10): open a file, write to it, unlink it, then read the bytes back through the open descriptor and assert `fstat` reports `st_nlink == 0`. Repeat with a child process holding the descriptor, so the scheduler orders the unlink against the child's reads.
- T5 (covers F11, F12): in a sticky directory owned by a second mapped uid, assert `EPERM` for removing an entry the guest does not own. Assert `EBUSY` for removing a mount point the guest can see. If the guest cannot reach a second uid or a mount point, the test fails and says so rather than skipping.
- T6 (covers F13, F14, F15, F16): assert `ENOENT` for a missing name and for `""`; `ENOTDIR` for a regular file used as an intermediate component; `EACCES` under a directory the guest cannot write; `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes; `ELOOP` for a symlink loop in the prefix; and `EFAULT` for `pathname = (char *)1`.
  Existing partial coverage via glibc: `tests/c/linkat_flags.c:58` (a missing name).
- T7 (covers F17, F20): assert `EROFS` under a read-only mount the guest can see; `ENOTDIR` for a relative path with a regular-file descriptor; `EBADF` for a relative path with a closed descriptor number.
- U1 (covers F22; host-config): the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, which the guest lacks, so CI cannot produce this error from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:736`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/506: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/527 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843; same stable-state assumption as unlink/rmdir/mkdirat at syscall_classification.rs:725); merged duplicate https://github.com/rrnewton/hermit/issues/527, whose CONDITIONAL is reconciled to deterministic; 21 flag behaviours, 7 tests; no coverage token, because tests/c/linkat_flags.c reaches unlinkat only through glibc, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount layout for EBUSY, host inode attributes); F11 narrowed to the sticky-directory rule, new F22 immutable or append-only EPERM (host-config U1); no open question is answered by these decisions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `unlinkat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+unlinkat&body=Discussion+for+the+%60unlinkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funlinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60unlinkat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: unlinkat" (https://github.com/rrnewton/hermit/issues/506), legacy variant A1. Manual reference: [unlinkat(2)](https://man7.org/linux/man-pages/man2/unlinkat.2.html).

Merged duplicate: https://github.com/rrnewton/hermit/issues/527, filed 2026-07-24 as "Syscall classification: unlinkat" (legacy variant A1). Comment histories: https://github.com/rrnewton/hermit/issues/506 (this issue) and https://github.com/rrnewton/hermit/issues/527 (0 comments when it was retired).

Merge reconciliation:

- Determinism: this issue said **deterministic** ("Same as unlink/rmdir; deterministic given fixed FS state"); the duplicate said **CONDITIONAL** ("The requested filesystem mutation is input-determined. Results depend on external filesystem state, so forwarding is consistent with Hermit's filesystem limitation."). Deterministic wins, for the reasons under Determinism analysis: the external-state dependency is the stable-state assumption the whole Detcore group shares, and the siblings carry `deterministic` under it.
- Nondeterminism sources: this issue said "None from the syscall; only external concurrent FS mutation"; the duplicate listed "Directory-fd identity, flags, permissions, path state, mounts, and concurrent mutations". Both are kept above: external mutation and mounts as sources, the rest as guest-controlled inputs.
- Semantics: both said `AT_REMOVEDIR` selects directory removal and `flags` 0 gives unlink behaviour (F1, F3). Both recorded success removing a temporary file relative to `AT_FDCWD`; both strace lines are kept below.
- Both recommended PASS-THRU; kept.

### 2026-10-03 host probes

Run by syscall-tracking-p3 on the measurement host (Linux 7.1.3, glibc 2.34, btrfs, unprivileged uid). This was the host, not Hermit.

- `strace -f` of coreutils `rm file` showed `unlinkat(AT_FDCWD, path, 0) = 0`.
- Probes against scratch entries in a scratch directory: a file 0; a directory with `flags` 0 `EISDIR`; `AT_REMOVEDIR` on a file `ENOTDIR`; on a non-empty directory `ENOTEMPTY`; `AT_REMOVEDIR` `"."` `EINVAL`; `"m1/.."` `ENOTEMPTY`; `"file/"` with `flags` 0 `ENOTDIR`; `AT_REMOVEDIR` `"m2/"` 0; a symlink to a directory with `flags` 0 removed the link and left the directory; `flags` 0x1, 0x100 and 0x1000 `EINVAL`; `pathname = (char *)1` `EFAULT`.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=unlinkat` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `unlinkat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
unlinkat(AT_FDCWD, "/tmp/sysclass45/sl2", 0) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Duplicate https://github.com/rrnewton/hermit/issues/527 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=unlinkat`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `unlinkat` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
unlinkat(AT_FDCWD, "/tmp/hermit-b5-unlink-Ovjdeh", 0) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
