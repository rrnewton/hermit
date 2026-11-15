---
schema: syscall-pages/v1
syscall: fchmodat2
nr_x86_64: 452
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/351
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchmodat2&body=Discussion+for+the+%60fchmodat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchmodat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchmodat2%60%2C+or+in+what+the+page+says%3F%0A
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

`fchmodat2(dirfd, pathname, mode, flags)` changes a file's permission bits. It resolves `pathname` relative to `dirfd`, and the kernel itself handles `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH`, which `fchmodat` cannot do and glibc otherwise emulates. Hermit forwards it to the host kernel unchanged. The result depends on the guest-visible filesystem and on the guest's identity in its user namespace, both of which are fixed for a given container image under ptrace.

## Linux semantics

Added in Linux 6.6. The kernel rejects unknown `flags` bits first, before it looks at the path or the mode. It then resolves the path (following a final symlink unless `AT_SYMLINK_NOFOLLOW` is given), takes write access to the mount, and sets the inode mode to `(mode & 07777)` combined with the file-type bits it already has. The process umask is not applied. The caller must own the file or hold `CAP_FOWNER` over it. The change goes through `notify_change`, which updates the inode's ctime and refuses immutable and append-only inodes. The call returns 0 or a negative errno and copies nothing back to the guest. `fchmodat` (268) is the older entry: it has no flags argument, and glibc implements its `AT_SYMLINK_NOFOLLOW` in userspace through an `O_PATH` descriptor.

### Flags and options

- F1: `mode` permission bits (`07777`, including `S_ISUID`, `S_ISGID` and `S_ISVTX`) replace the file's existing ones. Bits above `07777` are ignored and the umask is not applied.
- F2: `S_ISGID` is cleared from the new mode when the caller is not in the file's group and lacks `CAP_FSETID` over the inode; `S_ISUID` and `S_ISVTX` are kept as given
- F3: `flags == 0`: a symlink as the last component is followed and its target is changed
- F4: `AT_SYMLINK_NOFOLLOW` (0x100): a non-symlink last component is changed as usual. A symlink last component gives `EOPNOTSUPP`, because Linux symlinks have no settable mode.
- F5: `AT_EMPTY_PATH` (0x1000): an empty `pathname` changes the file `dirfd` refers to, including an `O_PATH` descriptor. Without the flag, an empty `pathname` gives `ENOENT`.
- F6: any other `flags` bit: `EINVAL`, checked before the path is resolved
- F7: `dirfd`: `AT_FDCWD` resolves a relative path against the working directory and a directory fd resolves against that directory. An absolute path ignores `dirfd`. A relative path with an invalid `dirfd` gives `EBADF`, and with a non-directory `dirfd` gives `ENOTDIR`.
- F8: permission: `EPERM` when the caller neither owns the file nor holds `CAP_FOWNER` over it. A root caller in a user namespace has no capability over files whose owner is not mapped into that namespace. `EPERM` also for an immutable or append-only inode.
- F9: `EROFS` when the file is on a read-only mount
- F10: path errors: `ENOENT`, `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, `EFAULT`, and `EACCES` when search permission on a prefix directory is denied
- F11: a successful call sets the inode's ctime to the current time, which the guest sees through `stat`
- F12: kernels before 6.6 have no `fchmodat2` and return `ENOSYS`; glibc then falls back to `fchmodat` and its userspace emulation

## Determinism analysis

The result and the new mode depend on the inputs listed below: the arguments, the filesystem state along the path (existence, type, owner, group, immutable flag), the caller's credentials, and the mount flags. Nothing depends on timing, and Hermit runs guest threads one at a time, so a chmod racing another guest thread's rename or unlink happens in a fixed order. The mode written is exactly `mode & 07777` (after F2), so for a fixed container image and fixed credentials the call and its later `stat` results repeat. The one time-dependent output is the ctime from F11, and that is visible only through `stat`, whose timestamps Hermit handles separately.

### Nondeterminism sources

- External path or metadata mutation
- Filesystem and LSM policy
- Kernel support for flag combinations

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `fchmodat2` is classified `PassThrough` at `detcore/src/syscall_classification.rs:775`, in the block whose comment at `detcore/src/syscall_classification.rs:767` and `detcore/src/syscall_classification.rs:768` says that "Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image"; `detcore/src/syscall_classification.rs:769` still carries `TODO-HUMAN-REVIEW(#683)` on that boundary. The comment at `detcore/src/lib.rs:2838` to `detcore/src/lib.rs:2840` says `fchmodat2` is untyped in the pinned Reverie revision and reaches the blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843`. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists it at `detcore/src/syscall_classification.rs:2051` and asserts `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough:

- Hermit has no virtual filesystem; the container's directory tree is the state. Only the kernel can apply a mode change that later `open`, `execve` and `stat` calls all see the same way.
- Under ptrace the guest is uid 0 in a user namespace with one mapped uid, so files the guest creates are owned by its own root and F8 lets it change them, as it would on a real root system. The result already repeats for a fixed image (see Determinism analysis), so a model would have nothing to fix.
- The owner and group are not set by this call. The case that forced the `chown` family off passthrough does not arise here: those calls were made deterministic no-ops (`detcore/src/syscall_classification.rs:679` to `detcore/src/syscall_classification.rs:688`) because the host identity cannot really change ownership.

Two consequences of other policies, both deterministic:

- Because guest `chown` is a no-op, every file the guest created keeps its mapped owner, so a later `fchmodat2` on it succeeds. On Linux the same sequence also succeeds for root, because root holds `CAP_FOWNER`.
- Files owned by host uids not mapped into the namespace give `EPERM` (F8). That repeats on one host, but can differ between hosts or images.

Code facts show `subscribed_record_replay: false`, so with `--passthru-opt` the call never reaches Detcore. `hermit record` uses its own subscriptions in a recorder module that is not in the exported tree, so this body does not describe how a recording captures the call.

The open gap is test evidence. The tests listed below call `fchmodat2`, but none checks the full flag list, and the strict trace in `hermit-cli/tests/syscall_file_metadata.rs` does not assert that `fchmodat2` reached the kernel.

## Recommended tests

- T1 (covers F1, F3): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fchmodat2, ...)` with flags 0. Set mode 0640 on a guest-created file, then 0600, then `07777 | 0170000`. Each time, assert with `stat` that the permission bits are exactly `mode & 07777` and the file type is unchanged. With umask 077, assert that 0666 is applied unchanged. Through a symlink to the file, assert that the target's mode changes and the link's own `lstat` mode does not.
  Existing partial coverage: `tests/c/fchmodat2_flags.c:54` (0640) and `tests/c/fchmodat2_flags.c:58` (0600); cell `c-programs/fchmodat2-flags` at `tests/e2e/manifests/c-programs.yaml:15447`, verify mode with ptrace enabled and `ci: true`. Also `tests/c/syscall_file_metadata.c:153` (0600, accepting `ENOSYS`), driven on ptrace by `deterministic_file_metadata_syscalls_verify` at `hermit-cli/tests/syscall_file_metadata.rs:51`.
- T2 (covers F2): on a guest-created file whose group is the guest's own, set `02755` and assert `S_ISGID` is kept. Change the group to one the caller is not in, then set `02755` and assert `S_ISGID` is cleared. If the namespace has no second mapped group, the test must print that and fail rather than skip. Set `04755` and `01755` and assert `S_ISUID` and `S_ISVTX` are kept.
- T3 (covers F4): with `AT_SYMLINK_NOFOLLOW`, assert that a regular file changes to 0644 and that a symlink gives `EOPNOTSUPP`.
  Existing partial coverage: `tests/c/fchmodat2_flags.c:62` (regular file).
- T4 (covers F5): open a file with `O_PATH`, call with `""` and `AT_EMPTY_PATH`, and assert 0 and the new mode. Without the flag, assert `ENOENT`.
- T5 (covers F6): assert `EINVAL` for `flags = 0xFFFF` and for `flags = 0x200`. Assert `EINVAL`, not `EFAULT`, when `flags` is invalid and `pathname` is a bad pointer, so the kernel's order of checks is pinned.
  Existing partial coverage: `tests/c/fchmodat2_flags.c:75` (`flags = 0xFFFF`).
- T6 (covers F7): assert that a relative path resolves against the working directory after `chdir` (`AT_FDCWD`) and against an open directory fd. Assert that an absolute path with `dirfd = -1` succeeds, a relative path with `dirfd = -1` gives `EBADF`, and a relative path with a regular-file `dirfd` gives `ENOTDIR`.
- T7 (covers F8, F9): assert `EPERM` for a file owned by a host uid that is unmapped in the guest's namespace, and `EROFS` for a file on a read-only mount the guest can see. Record which paths are used. Immutable files need `CAP_LINUX_IMMUTABLE` in the initial namespace, so the test should state that the immutable case is not reachable under ptrace rather than pass without testing it.
- T8 (covers F10): assert `ENOENT` for a missing path, `ENOTDIR` for a regular file used as a prefix, `ENAMETOOLONG` for a 256-byte component, `ELOOP` for a symlink loop with flags 0, and `EFAULT` for `pathname = (char *)1`.
  Existing partial coverage: `tests/c/fchmodat2_flags.c:70` (missing path).
- T9 (covers F11): record `st_ctim` before and after a successful call, assert that it does not go backwards and that both `--verify` runs print the same `stat` results.
- T10 (covers F12): on the CI kernel (6.6 or later), assert that `fchmodat2` never returns `ENOSYS` under Hermit, and add `fchmodat2` to the strict trace assertion list in `hermit-cli/tests/syscall_file_metadata.rs:91` to `hermit-cli/tests/syscall_file_metadata.rs:99`. Today `tests/c/syscall_file_metadata.c:154` accepts `ENOSYS`, so that test passes even on a kernel without the call.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/351#issuecomment-5872977169
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/351: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fchmodat2 records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/351 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843; the kernel applies the mode in the fixed container image and the guest is root in its user namespace); listed 12 flag behaviours and 10 tests; tests/c/fchmodat2_flags.c covers part of F1, F4, F6 and F10 but no coverage token is recorded, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fchmodat2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchmodat2&body=Discussion+for+the+%60fchmodat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchmodat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchmodat2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fchmodat2" (https://github.com/rrnewton/hermit/issues/351), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/351.

Legacy summary lines (2026-07-24): "Rank 72 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls." and "Manual reference: Linux man-pages chmod(2) / fchmodat2."

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fchmodat2 -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(452, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fchmodat2(-1, 0xffffffffffffffff, 0177777, AT_SYMLINK_NOFOLLOW|AT_EMPTY_PATH|0xffffeeff) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/351#issuecomment-5872977169
