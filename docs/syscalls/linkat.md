---
schema: syscall-pages/v1
syscall: linkat
nr_x86_64: 265
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1952
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+linkat&body=Discussion+for+the+%60linkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60linkat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 1
---

## Summary

`linkat(olddirfd, oldpath, newdirfd, newpath, flags)` creates a hard link like `link`, with each path resolved against its own directory descriptor and with flags that choose whether a symlink is followed and whether `olddirfd` itself is the object to link. Hermit forwards it to the host kernel unchanged. The result repeats under Hermit's private, stable container filesystem, except where host-wide settings decide whether a link is allowed: the `fs.protected_hardlinks` sysctl and, for `AT_EMPTY_PATH`, the host kernel's permission rule.

## Linux semantics

The kernel resolves `oldpath` against `olddirfd` and `newpath` against `newdirfd` (`AT_FDCWD` means the working directory; an absolute path ignores its descriptor). By default it does not follow a symlink in the last component of `oldpath`; `AT_SYMLINK_FOLLOW` makes it follow. `AT_EMPTY_PATH` with an empty `oldpath` links the object `olddirfd` refers to, which is how an `O_TMPFILE` file is given a name. Any other flag bit is rejected with `EINVAL`. From there the call is `link`: `newpath` must not exist and must be on the same mount as the source; on success both names refer to one inode, whose link count goes up by one and whose ctime changes, and the new parent's mtime and ctime change. The call returns 0 or a negative errno and writes nothing to guest memory. Before linking, the kernel's `may_linkat` check applies the `fs.protected_hardlinks` sysctl: when it is 1, a caller that does not own the inode (and lacks `CAP_FOWNER` over its owner) may link it only if it is a regular file that is not setuid, not setgid-executable, and is readable and writable by the caller. Errors are otherwise checked in path-walk order, so the errno is a function of the descriptors, the two paths, the flags, the filesystem state, the credentials, the mounts and that sysctl.

### Flags and options

- F1: directory descriptors: `AT_FDCWD` resolves a relative path against the working directory; any other descriptor must be an open directory. A relative path with a descriptor that is not open: `EBADF`. A relative path with a descriptor that is open but not a directory: `ENOTDIR`. An absolute path ignores its descriptor. The two descriptors may name different directories.
- F2: `flags = 0` and `newpath` does not exist: a second name for the inode is created, `st_nlink` goes up by one, and both names show the same `st_ino`
- F3: `flags = 0` and `oldpath` names a symlink: the symlink itself is linked, not its target
- F4: `AT_SYMLINK_FOLLOW`: a symlink in the last component of `oldpath` is followed, so the new name is a second link to the target. A dangling symlink then gives `ENOENT`, and a symlink loop gives `ELOOP`.
- F5: `AT_EMPTY_PATH` with `oldpath = ""`: the object `olddirfd` refers to is linked, including an `O_TMPFILE` file that has no name yet (an `O_TMPFILE` file opened with `O_EXCL` cannot be linked), when the host kernel's permission rule allows it (F15). Linking a `/proc/self/fd/N` path with `AT_SYMLINK_FOLLOW` reaches the same object without `AT_EMPTY_PATH`.
- F6: any flag bit other than `AT_SYMLINK_FOLLOW` and `AT_EMPTY_PATH`: `EINVAL`
- F7: `newpath` already exists as any type, including a dangling symlink: `EEXIST`
- F8: the source is a directory: `EPERM`
- F9: the source and `newpath` are on different mounts, including two bind mounts of one filesystem: `EXDEV`
- F10: `fs.protected_hardlinks` is 1 and the caller neither owns the inode nor has `CAP_FOWNER` over its owner, and the inode is not a regular file that the caller can read and write (or it is setuid, or setgid and group-executable): `EPERM`. With the sysctl at 0 the same call succeeds.
- F11: trailing slash on `oldpath` naming a non-directory: `ENOTDIR`; on an absent `newpath`: `ENOENT`. `oldpath` is missing, either path is empty without `AT_EMPTY_PATH`, or `newpath`'s parent is missing: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F12: write permission is denied on `newpath`'s parent, or search permission on a path component: `EACCES`.
- F13: a component is longer than `NAME_MAX`, or a path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in a prefix: `ELOOP`. Either path points outside the address space: `EFAULT`.
- F14: `newpath` lies under a read-only mount the guest can see: `EROFS`
- F15: host-config: the `AT_EMPTY_PATH` permission rule depends on the host kernel. Older kernels require `CAP_DAC_READ_SEARCH` in the initial user namespace, which the guest never has, and otherwise return `ENOENT`; recent kernels also allow it when the descriptor was opened with the caller's own credentials (measured on the measurement host, Linux 7.1.3: an unprivileged caller linked its own `O_TMPFILE` descriptor, and got `EPERM` for a directory descriptor).
- F16: host-config failure: the inode is immutable or append-only: `EPERM`. Setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks, so only files the host flagged give it.
- F17: host-config failures: `EMLINK` when the inode's link count is at the limit of the host filesystem type (65000 on ext4, far higher on XFS or tmpfs), and `EOVERFLOW` on an idmapped host mount on which the inode's owner has no mapping
- F18: host-transient failures: `ENOSPC` (the directory needs a new block), `EDQUOT` (quota) and `EIO`
- F19: host-config failure: the host filesystem holding the file does not support hard links: `EPERM`

## Determinism analysis

`conditional`. The result is a function of the two directory descriptors, the flags, the filesystem state along both paths, the caller's credentials, the mount layout, and two host inputs: the `fs.protected_hardlinks` sysctl (F10) and the kernel's `AT_EMPTY_PATH` permission rule (F15). Nothing in the call depends on timing. The kernel makes the link atomic, and Hermit runs threads one at a time, so two guest threads creating the same name, or racing a `rename` or `unlink` at either endpoint, get a fixed order. The descriptors name directories or files the guest opened earlier, so they are guest state. The filesystem, credentials and mounts are container state that Hermit holds fixed from run to run on one host; the mount layout can still differ between hosts (Cross-host variation, F9). The two host inputs are not fixed even on one image: they belong to the host, so the same guest on the same image can get 0 on one host and `EPERM` or `ENOENT` on another. Under ptrace the guest is root in a user namespace, so files owned by host uids outside the mapping are exactly the ones F10 applies to, and the guest never has `CAP_DAC_READ_SEARCH` in the initial user namespace for F15. On one host with an unchanged kernel and sysctl, the call repeats exactly. This matches `link` (https://github.com/rrnewton/hermit/issues/630), which is also `conditional`.

### Nondeterminism sources

- The host's `fs.protected_hardlinks` setting (F10), which differs between hosts and can be changed by the host administrator between runs.
- The host kernel version, which decides the `AT_EMPTY_PATH` permission rule (F15).
- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F16 and F17: inode flags, filesystem link limits, idmapped mounts. Host resources behind F18: free space, quota, storage errors.
- Values the call changes but does not return: the inode's link count and ctime, and the parent's timestamps. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- `fs.protected_hardlinks` sysctl: the same link of a file owned by an unmapped uid gives `EPERM` on a host with the sysctl at 1 and succeeds on a host with it at 0 (F10).
- Host kernel version: linking a descriptor with `AT_EMPTY_PATH` gives `ENOENT` on older kernels and succeeds on recent ones for a descriptor the caller opened (F15).
- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F16).
- Filesystem type and mount setup under the guest's files: the link-count limit decides when `EMLINK` appears, and an idmapped host mount can give `EOVERFLOW` (F17).
- Mount layout the guest sees: two paths on one filesystem on one host can be on different mounts on another, so the same link succeeds on one and gives `EXDEV` on the other (F9). A host filesystem without hard-link support gives `EPERM` (F19).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `linkat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:729`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The block comment at `detcore/src/syscall_classification.rs:691` and `:692` calls these calls "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions". The group comment at `detcore/src/syscall_classification.rs:725` and `:726` gives the condition for `linkat`: these calls "repeat given stable guest-visible filesystem state with no external mutation".
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::linkat` at `detcore/src/syscall_classification.rs:2072` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`. The 2026-08-07 strategy draft (kept under Additional information) proposed resolving both endpoints through modelled fd and path state and updating modelled inode and link-count state. It is not adopted, for the same reasons the `link` and `chmod` bodies give:

- Hermit has no virtual filesystem and no inode model. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F14 the same way the guest's other path syscalls see them. A link-count model would have to agree with every `stat`, `open`, `unlink` and `rename` the kernel serves directly.
- The draft's worries are already handled. Path races between guest threads are serialized by Hermit's scheduler; the descriptors are guest descriptors that the kernel already holds, and the guest's descriptor table is reproduced by its earlier calls; host object identity reaches the guest only through `stat`-family calls, which are tracked there.
- Emulating the call would not remove the F10 and F15 conditions. Hermit would have to pick a `protected_hardlinks` value and a kernel rule, then fake the link or the refusal, which would disagree with the host kernel that serves every other path call on the same files.
- The return value carries no host-specific data: 0 or an errno that follows from the inputs. The two host inputs are conditions on where a recording or a verify run is compared, which is why `linux_determinism` is `conditional` while `target_policy` stays `passthrough`. Whether Hermit should record those host inputs and refuse or warn when they differ is an open question below.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `linkat`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `linkat`. No record/replay integration test issues `linkat` at this SHA.

Existing run-mode evidence: `tests/c/linkat_flags.c:46` calls glibc `linkat(AT_FDCWD, a, AT_FDCWD, b, 0)` and checks that `b` is a regular file with `st_nlink == 2` (`tests/c/linkat_flags.c:47`). `tests/c/linkat_flags.c:62` calls `linkat(..., AT_SYMLINK_FOLLOW)` on a symlink to a regular file and checks that the new name is a regular file (`tests/c/linkat_flags.c:63`). Both use `AT_FDCWD` with absolute paths. Cell `c-programs/linkat-flags` at `tests/e2e/manifests/c-programs.yaml:12263` runs it in verify mode with `ci: true`. On this host's glibc 2.34, glibc `linkat()` issues `SYS_linkat` (host check under Additional information). `tests_naming` is 0: no test calls `syscall(SYS_linkat, ...)`.

No coverage token is recorded yet: the existing cell covers F2 and the regular-target case of F4 through the libc wrapper, and the remaining behaviours have no test.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call raw `syscall(SYS_linkat, ...)` with `AT_FDCWD` and relative paths, with two different real directory descriptors, and with absolute paths and an invalid descriptor (which must be ignored). Assert that both names show the same `st_ino`, that `st_nlink` goes from 1 to 2, and that writing through one name is visible through the other. Assert `EBADF` for a relative path with descriptor 999 and `ENOTDIR` for a relative path with a regular-file descriptor.
  Existing partial coverage: `tests/c/linkat_flags.c:46` (glibc wrapper, `AT_FDCWD`, absolute paths; cell `c-programs/linkat-flags`).
- T2 (covers F3, F4): with `flags = 0`, link a symlink and a dangling symlink and check with `lstat` that the new name is a symlink with the same `st_ino`. With `AT_SYMLINK_FOLLOW`, link a symlink to a regular file and check that the new name has the target's `st_ino`; assert `ENOENT` for a dangling symlink and `ELOOP` for a symlink loop.
  Existing partial coverage: `tests/c/linkat_flags.c:62` (glibc wrapper, symlink to a regular file).
- T3 (covers F5): open an `O_TMPFILE` file in a guest-writable directory, write to it, and link it with `AT_EMPTY_PATH`; assert the result and print it, so that a verify run on a host with a different kernel rule shows the difference. Link the same kind of file through `/proc/self/fd/N` with `AT_SYMLINK_FOLLOW` and assert the new name has the written content. Assert that an `O_TMPFILE | O_EXCL` file cannot be linked. The `AT_EMPTY_PATH` result depends on the host kernel (F15, recorded as U2); the `/proc/self/fd/N` link is the assertion every host must pass.
- T4 (covers F6, F7): assert `EINVAL` for `flags = 0x1` and for `AT_SYMLINK_NOFOLLOW`. Assert `EEXIST` for an existing file, an existing directory and a dangling symlink at `newpath`.
- T5 (covers F8, F9): assert `EPERM` for a directory source. Assert `EXDEV` between two distinct guest-visible mounts. Record the chosen paths in the test.
- T6 (covers F10): read `/proc/sys/fs/protected_hardlinks` and print it. Link a file owned by an unmapped host uid that the guest cannot both read and write, on the same mount as a writable directory. Assert `EPERM` when the sysctl is 1 and 0 when it is 0.
- T7 (covers F11): assert `ENOTDIR` for `oldpath = "f/"` and `ENOENT` for `newpath = "g/"` with `g` absent. Assert `ENOENT` for a missing `oldpath`, for `""` without `AT_EMPTY_PATH` and for a missing parent of `newpath`, and `ENOTDIR` when a regular file is used as an intermediate component.
- T8 (covers F12): assert `EACCES` when `newpath`'s parent is a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. The immutable or append-only case is F16, recorded as U3; this test does not cover it.
- T9 (covers F13): assert `ENAMETOOLONG` for a 256-byte component and for a path over 4096 bytes in each argument, `ELOOP` for a symlink loop in a prefix, and `EFAULT` for `oldpath = (char *)1` and for `newpath = (char *)1`.
- T10 (covers F14): assert `EROFS` for a link under a read-only mount the guest can see. `EMLINK` and `EOVERFLOW` (F17) are recorded as U3, and `ENOSPC`, `EDQUOT` and `EIO` (F18) as U4; this test does not provoke them.
- T11 (covers F2, F7): in two guest threads, race `linkat` to the same `newpath` against `rename` and `unlink` of the source, and require the same winner and errno across repeated `--verify` runs.
- U1 (covers F10; host-config): `fs.protected_hardlinks` is a host sysctl the guest cannot set, so one CI host exercises only one of its two answers; T6 records which. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:729`, `detcore/src/lib.rs:2843`).
- U2 (covers F15; host-config): the `AT_EMPTY_PATH` permission rule belongs to the host kernel, so one CI host exercises only one rule. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:729`, `detcore/src/lib.rs:2843`).
- U3 (covers F9, F16, F17, F19; host-config): the mount layout the guest sees, immutable and append-only flags, the filesystem's link-count limit and hard-link support, and idmapped mounts belong to the host, so which paths give `EXDEV`, and whether the other errors appear at all, depends on the CI host; T5 still checks `EXDEV` for one pair of distinct mounts. Hermit forwards them unchanged (`detcore/src/syscall_classification.rs:729`, `detcore/src/lib.rs:2843`).
- U4 (covers F18; host-transient): `ENOSPC`, `EDQUOT` and `EIO` come from host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F18 counts toward done only once Hermit hides these errors or reports them as infrastructure errors.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. linkat is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1952: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: set conditional/passthrough (target_policy investigate -> passthrough: classified at detcore/src/syscall_classification.rs:729, forwarded at detcore/src/lib.rs:2843; the 2026-08-07 inode/link-count model draft is not adopted because Hermit has no filesystem model); conditional because fs.protected_hardlinks and the host kernel's AT_EMPTY_PATH rule are host inputs; listed 14 flag behaviours and 11 tests from the legacy required tests; related now names link; no coverage token recorded because no test calls SYS_linkat directly, so tracking_status stays todo; open questions for the owner on pinning those host inputs and on unreachable host errnos
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (fs.protected_hardlinks sysctl, mount layout, host kernel AT_EMPTY_PATH rule, inode flags on host files, filesystem link limit and idmapped mounts); host-config U1 for F10; EXDEV (F9) added to host-config U3; hard-link-support EPERM split from F8 as new F19 (host-config U3); the kernel-dependent AT_EMPTY_PATH rule split from F5 as new F15 (host-config U2); immutable EPERM split from F12 as new F16; F14 narrowed to EROFS with EMLINK and EOVERFLOW split out as new F17 (host-config U3) and ENOSPC, EDQUOT and EIO as new F18 (host-transient U4); T3, T8 and T10 adjusted; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `linkat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+linkat&body=Discussion+for+the+%60linkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60linkat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: linkat" (https://github.com/rrnewton/hermit/issues/1952), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- F10 and F15: should Hermit treat `fs.protected_hardlinks` and the kernel's `AT_EMPTY_PATH` rule as part of a run's environment, for example by recording them and refusing or warning on replay or verify when they differ, or is a documented host precondition enough? This is the same question the `link` body (https://github.com/rrnewton/hermit/issues/630) asks, and it decides whether `linux_determinism` can ever move from `conditional` under Hermit.

### Legacy strategy, required tests and acceptance text (2026-08-07)

Legacy determinism text: "linkat mutates namespace and inode link counts using two dirfd-relative paths and flags such as AT_EMPTY_PATH and AT_SYMLINK_FOLLOW. Host object identity, permissions, mount boundaries, and path races affect the outcome." Kept above: the flags are F4 and F5, permissions F10 and F12, mount boundaries F9, and path races are serialized by Hermit's scheduler (T11).

Strategy draft (not adopted; see Hermit design): "Resolve both endpoints through deterministic fd/path state, serialize namespace mutation, and update modeled inode/link-count state atomically. Implement flag, cross-mount, and permission errors explicitly."

Legacy required tests, now T1-T11:

- Cover AT_FDCWD and real dirfds, existing destinations, missing sources, symlinks, AT_EMPTY_PATH, and cross-mount errors. (T1, T2, T3, T4, T5, T7)
- Verify inode identity, link counts, directory entries, and exact errno. (T1, T2, T4 to T10)
- Race rename/unlink/create at both endpoints under scheduler control. (T11)
- Compare repeated strict runs and record/replay across supporting backends. (every T runs under `--verify`; ptrace only under owner Q3)

Legacy acceptance text: "Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy." The v1 done rule in Done criteria replaces it; the target policy is now passthrough, so no move out of the pass-through table is planned.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

linkat is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### 2026-10-03 host checks

On the measurement host (Linux 7.1.3, glibc 2.34, x86_64), run as an unprivileged host user with `fs.protected_hardlinks = 1`:

- `strace` of glibc `linkat(dirfd, "b", dirfd, "c", 0)` showed `linkat(3, "b", 3, "c", 0) = 0`: the wrapper issues `SYS_linkat`.
- Raw `syscall(SYS_linkat, dirfd, "b", dirfd, "x", 0x1)` returned `EINVAL` (strace: `linkat(3, "b", 3, "x", 0x1 /* AT_??? */) = -1 EINVAL`).
- `linkat(tmpfd, "", AT_FDCWD, ".../t1", AT_EMPTY_PATH)` on the caller's own `O_TMPFILE` descriptor returned 0; the same call on a directory descriptor returned `EPERM`; `linkat(AT_FDCWD, "/proc/self/fd/N", AT_FDCWD, ".../t2", AT_SYMLINK_FOLLOW)` on an `O_TMPFILE` descriptor returned 0.
