---
schema: syscall-pages/v1
syscall: fchmodat
nr_x86_64: 268
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1943
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchmodat&body=Discussion+for+the+%60fchmodat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchmodat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchmodat%60%2C+or+in+what+the+page+says%3F%0A
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

`fchmodat(dirfd, pathname, mode)` is `chmod` with `pathname` resolved against the directory descriptor `dirfd`. The raw syscall has no flags argument and always follows a symlink in the last component; the libc wrapper's `flags` argument is handled in user space or by `fchmodat2`. Hermit forwards the syscall to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and on the fixed container credentials, and Detcore keeps no mode state of its own.

## Linux semantics

The kernel resolves `pathname` against `dirfd` (`AT_FDCWD` means the working directory; an absolute path ignores the descriptor), following symlinks in every component including the last, so `fchmodat` on a symlink changes its target and a dangling symlink gives `ENOENT`. It then checks that the caller may change the mode: its filesystem uid must own the inode, or it must hold `CAP_FOWNER` over the inode's owner. In a user namespace the capability counts only when the inode's uid and gid are mapped into it. The new mode replaces the low twelve bits (`07777`) of the old one; it is not combined with it, and the umask does not apply. Only the S_ISGID adjustment (F2) and, where an access ACL exists, its mask entry (F5) change what is stored. On success the inode's ctime is set to the current time. The call returns 0 or a negative errno and never reports the resulting mode, which a guest sees only through the stat family.

The x86_64 syscall takes three arguments. The glibc prototype `fchmodat(dirfd, pathname, mode, flags)` has a fourth, which the kernel never sees through this syscall: a fourth register value is ignored. Since Linux 6.6 the flags-bearing variant is `fchmodat2` (https://github.com/rrnewton/hermit/issues/351). On the measurement host (glibc 2.34), the wrapper with `flags = 0` issues `SYS_fchmodat`; with `AT_SYMLINK_NOFOLLOW` it opens the path with `O_PATH | O_NOFOLLOW` and then calls `chmod("/proc/self/fd/N")` for a non-symlink, or fails with `EOPNOTSUPP` for a symlink without a mode-changing syscall; with any other flag it fails with `EINVAL` without a syscall (host checks under Additional information). Newer glibc releases try `fchmodat2` first.

Manual reference: [fchmodat(2)](https://man7.org/linux/man-pages/man2/chmod.2.html).

### Flags and options

- F1: mode bits: `mode & 07777` replaces the permission bits (`0777`), `S_ISUID`, `S_ISGID` and `S_ISVTX`; file-type bits in `mode` are ignored. A second call fully replaces the first rather than OR-ing with it.
- F2: S_ISGID clearing: if the caller is not in the file's group (filesystem gid or supplementary groups) and lacks `CAP_FSETID` over the inode, the kernel silently drops `S_ISGID` from the new mode and still returns 0.
- F3: `dirfd`: `AT_FDCWD` resolves a relative `pathname` against the working directory, inside the caller's root and mount namespace; any other descriptor must be an open directory. A relative path with a descriptor that is not open: `EBADF`. A relative path with a descriptor that is open but not a directory: `ENOTDIR`. An absolute path ignores the descriptor. An empty `pathname` gives `ENOENT`; the raw syscall has no `AT_EMPTY_PATH`.
- F4: no flags in the raw syscall: every component, including the last, is followed, and a fourth register value is ignored. A symlink's own mode is never changed. A dangling last component gives `ENOENT`, and too many symlinks give `ELOOP`. The libc wrapper's `AT_SYMLINK_NOFOLLOW` and invalid-flag behaviour happen in user space or in other syscalls (`chmod` on `/proc/self/fd/N`, or `fchmodat2`), not in this one.
- F5: POSIX ACLs: when the file has an access ACL, the group bits of `mode` become the ACL mask entry (or the owning-group entry if there is no mask), so a later `getxattr("system.posix_acl_access")` changes too.
- F6: ownership: the caller's filesystem uid does not own the file and it lacks `CAP_FOWNER` over the inode: `EPERM`.
- F7: path errors: an intermediate component is missing: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`. Search permission is denied on a prefix: `EACCES`. A component longer than `NAME_MAX` or a path longer than `PATH_MAX`: `ENAMETOOLONG`. `pathname` outside the address space: `EFAULT`.
- F8: `EROFS`: the file is on a read-only mount in the guest's mount namespace
- F9: object types: regular files, directories, device nodes, FIFOs and sockets bound in the filesystem all take the new mode the same way. On a directory, `S_ISVTX` is the restricted-deletion flag and `S_ISGID` makes new entries inherit the directory's group.
- F10: host-config failure: the inode is immutable or append-only: `EPERM`. Setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest (root only in its own user namespace) lacks, so only files the host flagged give it.
- F11: host-transient failure: `EIO` from failing host storage.

## Determinism analysis

`conditional`. The result is a function of the descriptor, the path, the filesystem state along it, the inode's owner, group and flags, the caller's credentials and the mount flags. The descriptor names a directory the guest opened earlier, so it is guest state. The stored mode is a pure function of `mode` and those inputs. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so an `fchmodat` racing a `rename` or `unlink` of the same path in another guest thread resolves in a fixed order. The call repeats exactly when the guest-visible filesystem and the container credentials start in the same state and nothing outside the container changes them. That condition is why the classification is `conditional`: Hermit holds the filesystem stable by assumption, not by modelling it. This matches `chmod` (https://github.com/rrnewton/hermit/issues/1940).

### Nondeterminism sources

- Changes to the filesystem made outside the container, including a host process changing the file's owner or mode. Hermit does not try to make a changing host filesystem deterministic.
- Host resources and configuration: read-only remounts (F8), immutable or append-only inode flags (F10) and I/O errors (F11).
- The libc in the guest image decides which syscall a `fchmodat()` call with flags reaches (F4); that choice is fixed by the image, not by the host.
- A value the call changes but does not return: the inode's ctime. It becomes visible only through `stat` and similar calls, and is handled there.

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F10) on that host and not on another.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `fchmodat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:728`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The group comment at `detcore/src/syscall_classification.rs:725` and `:726` says these path mutations "repeat given stable guest-visible filesystem state with no external mutation". `fchmodat2` is passed through at `detcore/src/syscall_classification.rs:775`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::fchmodat` at `detcore/src/syscall_classification.rs:2050` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`. The negative case at `detcore/src/syscall_classification.rs:2817` of the test `ownership_change_family_is_determinized_not_passthrough` (`detcore/src/syscall_classification.rs:2785`) keeps `fchmodat` out of the ownership-change set that determinizes `chown`.

The 2026-08-07 strategy draft (kept under Additional information) proposed resolving `dirfd` and the path through deterministic filesystem state, applying mode changes to modelled object identity, and refusing combinations whose object binding cannot be verified. This body does not adopt it, for the same reasons the `chmod` body gives:

- Hermit has no virtual filesystem. Mode bits are inode state in the guest's container mount namespace, and the guest's later `stat`, `open` and `access` calls read them from the kernel. A Detcore-side mode model would have to answer all of those too, or it would disagree with them.
- The inputs the draft worried about are either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image, the fixed container identity) or out of scope by Hermit's stated boundary (host processes changing the filesystem). "fd translation" is not needed: the descriptor is a guest descriptor that the kernel already holds, and the guest's descriptor table is reproduced by its earlier calls. The pathname races it names are between guest threads, and the scheduler already serializes them.
- There is no flag combination to refuse: the raw syscall takes no flags (F4).
- There is a fidelity boundary, not a determinism one. `getuid` and related calls are emulated as virtual root 0 (`detcore/src/syscall_classification.rs:658`), while `stat` copies `st_uid` from the kernel (`detcore/src/stat.rs:163`). Under the default user namespace the guest owns the files it creates and the owner check (F6) passes. Under `--no-namespace`, where the comment at `detcore/src/syscall_classification.rs:680` says the host identity behind the guest is not root, a guest that believes it is root gets `EPERM` on files it does not own, where real root would succeed. That answer repeats run to run, so it does not change `linux_determinism`; owner decision 3 accepts it as reduced fidelity, as for `chmod` (below).

Record and replay. Code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `fchmodat` out. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module not in the exported tree, so this body does not describe how record captures `fchmodat`. No record/replay integration test issues `fchmodat` at this SHA.

Existing run-mode evidence: `tests/c/fchmod_bits.c:67` calls glibc `fchmodat(AT_FDCWD, path, 0640, 0)` and reads the mode back by path. Cell `c-programs/fchmod-bits` at `tests/e2e/manifests/c-programs.yaml:11761` runs it in verify mode with `ci: true`. On the measurement host glibc 2.34 that call issues `SYS_fchmodat`. `tests_naming` is 0: no test calls `syscall(SYS_fchmodat, ...)`, so a libc that switches to `fchmodat2` would quietly remove this coverage.

No coverage token is recorded yet: the existing cell covers part of F1 and F3, and the remaining behaviours have no test.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, the kernel checks the launching host user's uid, groups and capabilities while Detcore reports uid 0, so `fchmodat` on a file that user does not own fails with `EPERM` (F6) where real root would succeed, and whether `S_ISGID` is silently dropped (F2) follows the host user's groups.

## Recommended tests

- T1 (covers F1, F3): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fchmodat, ...)` with `AT_FDCWD` and a relative path, with a real directory descriptor and a relative path, and with an absolute path and an invalid descriptor (which must be ignored). Use modes `0640`, `0600`, `07777`, `S_IFDIR | 0644` and `0`, and assert the exact `st_mode & 07777` from `stat` after each call, including that a second call replaces rather than ORs. Assert `EBADF` for a relative path with descriptor 999, `ENOTDIR` for a relative path with a regular-file descriptor, and `ENOENT` for `""`.
  Existing partial coverage via the glibc wrapper: `tests/c/fchmod_bits.c:67` (cell `c-programs/fchmod-bits`).
- T2 (covers F2): `chown` a file the guest created to a second mapped group the guest is not a member of, drop `CAP_FSETID` from the effective set, and assert that `fchmodat(dirfd, name, 02755)` returns 0 and stores `0755`. With `CAP_FSETID` kept, assert it stores `02755`.
- T3 (covers F4): call raw `SYS_fchmodat` on a symlink to a file and assert the target's mode changes while `lstat` of the link still reports `0777`. Pass a non-zero fourth register value (for example `AT_SYMLINK_NOFOLLOW`) and assert the call still follows the link. Assert `ENOENT` through a dangling symlink and `ELOOP` through a symlink loop.
- T4 (covers F5): set an access ACL with a named-user entry and a mask through `setxattr("system.posix_acl_access")`, call `fchmodat` with `0640`, and assert the ACL mask entry becomes `r--`. The fixture directory must be on a filesystem that supports ACLs; if it does not, the test fails rather than skips.
- T5 (covers F6): assert `EPERM` for a file owned by a uid the guest's user namespace does not map. The immutable and append-only case is F10, recorded as U1 rather than tested.
- T6 (covers F7): assert `ENOENT` for a missing intermediate component, `ENOTDIR` with a regular file as an intermediate component, `EACCES` with a prefix directory lacking search permission for an unmapped owner, `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, and `EFAULT` for `pathname = (char *)1`.
- T7 (covers F8): assert `EROFS` under a read-only mount the guest can see. `EIO` is F11, recorded as U2 rather than tested.
- T8 (covers F9): call `fchmodat` on a directory with `01777` and `02775`, on a FIFO made with `mknod`, and on a bound `AF_UNIX` socket path, and assert each stored mode. Assert that a file created in the setgid directory takes the directory's group.
- T9 (covers F1, F4): race `fchmodat(dirfd, name)` in one guest thread against `rename` and `unlink` of `name` in another under `hermit run --verify`, and require the same outcome (errno and final mode) on both runs.
- T10 (covers F1): record/replay on ptrace: `hermit record` replays a raw `SYS_fchmodat` success and an `ENOENT` failure from the recording, and the replayed guest reads back the same mode.
- U1 (covers F10; host-config): only the host can set the immutable or append-only flag, so CI produces this `EPERM` only on a host that flagged a file the guest sees. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:728`, `detcore/src/lib.rs:2843`).
- U2 (covers F11; host-transient): `EIO` comes from failing host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F11 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. fchmodat is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1943: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: set conditional/passthrough (target_policy investigate -> passthrough: classified at detcore/src/syscall_classification.rs:728, forwarded at detcore/src/lib.rs:2843; the 2026-08-07 metadata-model draft is not adopted because Hermit has no filesystem model, and the raw syscall takes no flags to refuse); listed 9 flag behaviours and 10 tests from the legacy required tests; related now names chmod and fchmodat2; no coverage token recorded because no test calls SYS_fchmodat directly, so tracking_status stays todo; open question for the owner: under --no-namespace a guest shown uid 0 gets EPERM on files it does not own (fidelity, not determinism), shared with chmod
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files); immutable or append-only EPERM split from F6 into new F10 (host-config U1); EIO split from F8 into new F11 (host-transient U2), F8 keeps EROFS; T5 and T7 narrowed; the open question on unreachable host errors is resolved by decision 1
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (EPERM on files the host user does not own, F6, and host-group S_ISGID clearing under --no-namespace and namespace-less backends); target_policy unchanged (passthrough); no new F/T ids; removed the --no-namespace fidelity open question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fchmodat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchmodat&body=Discussion+for+the+%60fchmodat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchmodat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchmodat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: fchmodat" (https://github.com/rrnewton/hermit/issues/1943), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- The new ctime is outside this issue. It belongs to the stat family tracking.

### Legacy strategy, required tests and acceptance text (2026-08-07)

Legacy determinism text: "fchmodat changes metadata through dirfd-relative pathname resolution and optional symlink behavior. Results depend on host namespace, ownership, mount state, fd translation, and path-race timing." Kept above, except that the raw syscall has no optional symlink behaviour (F4) and no fd translation is needed (Hermit design).

Strategy draft (not adopted; see Hermit design): "Resolve dirfd and path through deterministic filesystem state, apply mode changes to modeled object identity, and implement supported flags with deterministic errors. Refuse combinations whose object binding cannot be verified."

Legacy required tests, now T1-T10:

- Cover AT_FDCWD and real dirfds, relative and absolute paths, symlinks, invalid flags, and empty/missing paths. (T1, T3, T6; invalid flags are a libc matter, F4)
- Check exact mode-bit results and errno for owned, unowned, and read-only cases. (T1, T2, T5, T7)
- Race rename/unlink with the operation and require scheduler-controlled repeatability. (T9)
- Compare strict repeats and record/replay across supporting backends. (T9, T10; ptrace only under owner Q3)

Legacy acceptance text: "Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy." The v1 done rule in Done criteria replaces it; the target policy is now passthrough, so no move out of the pass-through table is planned.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

fchmodat is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### 2026-10-03 host checks

On the measurement host (Linux 7.1.3, glibc 2.34, x86_64), traced with `strace -e trace=fchmodat,fchmodat2,chmod,openat`:

- glibc `fchmodat(dirfd, "b", 0600, 0)` issued `fchmodat(3, "b", 0600) = 0`.
- glibc `fchmodat(dirfd, "b", 0640, AT_SYMLINK_NOFOLLOW)` on a regular file issued `openat(3, "b", O_RDONLY|O_NOFOLLOW|O_CLOEXEC|O_PATH) = 4` then `chmod("/proc/self/fd/4", 0640) = 0`; on a symlink it issued only the `openat` and returned `EOPNOTSUPP`.
- glibc `fchmodat(dirfd, "b", 0640, 0x4)` returned `EINVAL` with no mode-changing syscall.
- raw `syscall(SYS_fchmodat, dirfd, "b", 0640, 0x12345)` returned 0 (strace shows three arguments), and glibc `fchmodat(dirfd, "", 0640, 0)` issued `fchmodat(3, "", 0640) = -1 ENOENT`.
