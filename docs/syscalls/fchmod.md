---
schema: syscall-pages/v1
syscall: fchmod
nr_x86_64: 91
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/350
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchmod&body=Discussion+for+the+%60fchmod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchmod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchmod%60%2C+or+in+what+the+page+says%3F%0A
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

`fchmod(fd, mode)` sets the permission and special mode bits of the inode that an open file descriptor refers to. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable and on the fixed container credentials; Detcore keeps no mode state of its own.

## Linux semantics

The kernel looks up `fd` in the caller's descriptor table and applies the same mode change as `chmod(2)` to the inode behind it, with no pathname resolution. The open mode of the descriptor does not matter: a descriptor opened `O_RDONLY` can change the mode, because the permission check is ownership, not access. The caller's filesystem uid must own the inode, or it must hold `CAP_FOWNER` over the inode's owner; in a user namespace that capability counts only when the inode's uid and gid are mapped. The new mode replaces the low twelve bits (`07777`) of the old one; it is not combined with it, and the umask does not apply. Only the S_ISGID adjustment (F2) and, where an access ACL exists, its mask entry (F5) change what is stored. On success the inode's ctime is set to the current time. The call returns 0 or a negative errno and never reports the resulting mode, which a guest sees only through the stat family.

Manual reference: [fchmod(2)](https://man7.org/linux/man-pages/man2/fchmod.2.html).

### Flags and options

- F1: mode bits: `mode & 07777` replaces the permission bits (`0777`), `S_ISUID`, `S_ISGID` and `S_ISVTX`; file-type and higher bits in `mode` are ignored (the 2026-07-24 trace below passes `0177777`). A second call fully replaces the first rather than OR-ing with it.
- F2: S_ISGID clearing: if the caller is not in the file's group (filesystem gid or supplementary groups) and lacks `CAP_FSETID` over the inode, the kernel silently drops `S_ISGID` from the new mode and still returns 0.
- F3: descriptor validity: a closed or out-of-range `fd`, or a descriptor opened with `O_PATH` (including an `O_PATH | O_NOFOLLOW` descriptor for a symlink), gives `EBADF`. The descriptor's access mode (`O_RDONLY`, `O_WRONLY`, `O_RDWR`) does not affect the result.
- F4: inode reached through the descriptor: the change applies to the open inode even after its last name is unlinked or renamed, and a later `stat` by any surviving path or `fstat` on any descriptor for the same inode sees it. Directories opened with `O_DIRECTORY` take the mode the same way, where `S_ISVTX` is the restricted-deletion flag and `S_ISGID` makes new entries inherit the directory's group. Descriptors for pseudo-filesystem inodes (pipes, sockets, anonymous inodes such as eventfd) get whatever that filesystem's setattr does.
- F5: POSIX ACLs: when the file has an access ACL, the group bits of `mode` become the ACL mask entry (or the owning-group entry if there is no mask).
- F6: ownership: the caller's filesystem uid does not own the inode and it lacks `CAP_FOWNER` over it: `EPERM`.
- F7: `EROFS` when the inode is on a read-only mount (including a descriptor opened before the mount became read-only).
- F8: host-config failure: the inode is immutable or append-only: `EPERM`. Setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest (root only in its own user namespace) lacks, so only inodes the host flagged give it.
- F9: host-transient failure: `EIO` from failing host storage.

## Determinism analysis

The result is a function of the descriptor table, the inode's owner, group and flags, the caller's credentials and the mount flags. The stored mode is a pure function of `mode` and those inputs, and nothing in the call depends on timing. Descriptor numbers are assigned by the kernel as the lowest free slot, and under Hermit guest threads run one at a time, so the same program reaches the same `fd` for the same inode, and an `fchmod` racing a `close` or `dup2` of that descriptor in another guest thread resolves in a fixed order. The call repeats when the guest-visible filesystem and the container credentials start in the same state and nothing outside the container changes them, which is why it is `conditional`. The 2026-07-24 trace below shows only the `EBADF` path and is not by itself evidence for the success path.

### Nondeterminism sources

- External metadata mutation: a host process changing the mode, owner or flags of the same inode.
- Filesystem ACL, immutable, mount, or security policy, all of which come from the host filesystem image and mount table (F5, F7, F8).
- Host ownership of inodes Hermit did not create: an inode in the launch image owned by a uid the guest's user namespace does not map gives `EPERM` (F6).
- The new ctime, which the call sets but does not return; it is visible only through the stat family, which owns its virtualization.

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F8) on that host and not on another.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `fchmod` is classified `PassThrough` at `detcore/src/syscall_classification.rs:773`, under the comment at `detcore/src/syscall_classification.rs:767` that "Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image", in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. That group carries `TODO-HUMAN-REVIEW(#683): Confirm the metadata/writeback passthrough boundary` at `detcore/src/syscall_classification.rs:769`; this body's answer for `fchmod` is that the boundary holds, for the reasons below. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `Sysno::fchmod` as `PassThrough` at `detcore/src/syscall_classification.rs:2049`, and the negative case at `detcore/src/syscall_classification.rs:2810` of `ownership_change_family_is_determinized_not_passthrough` keeps `fchmod` out of the ownership-change no-op set that determinizes `chown`.

Why forwarding is right:

- Hermit has no virtual filesystem. Mode bits are inode state in the guest's container mount namespace, and later `fstat`, `open` and `access` calls read them from the kernel. A Detcore-side mode model would have to answer all of those too.
- `fchmod` takes no path, so it has none of the pathname races `chmod` has. Its only guest-visible input besides the inode is the descriptor table, which the kernel allocates deterministically under Hermit's serialized execution.
- The inputs that could vary (inode ownership, flags, mount policy) are fixed container state or outside Hermit's stated boundary (host processes changing the filesystem).
- Fidelity boundary, shared with `chmod`: `getuid` is emulated as virtual root 0 (`detcore/src/syscall_classification.rs:658`), while `stat` copies `st_uid` from the kernel (`detcore/src/stat.rs:163`). Under `--no-namespace` (comment at `detcore/src/syscall_classification.rs:680`), a guest shown uid 0 gets `EPERM` from `fchmod` on an inode it does not own, where real root would succeed. The answer repeats run to run, so it does not affect `linux_determinism`; owner decision 3 accepts it as reduced fidelity (below).
- Record and replay: code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `fchmod` out. `hermit record` has its own subscriptions in a recorder module (comment at `detcore/src/lib.rs:1078`) that is not in the exported tree, and no test at this SHA records an `fchmod`.

Existing run-mode evidence, all through glibc `fchmod()`, which issues `SYS_fchmod` on the host checked (glibc 2.34, x86_64):

- `tests/c/fchmod_bits.c:52` sets `0640` and reads it back through the same descriptor; `tests/c/fchmod_bits.c:57` sets `0600` and checks the bits were replaced, not OR-ed. Cell `c-programs/fchmod-bits` (`tests/e2e/manifests/c-programs.yaml:11761`), verify mode with `ci: true`.
- `tests/c/chown_virtual_root_identity.c:127` sets `06755` and reads it back with `fstat`, and `tests/c/chown_virtual_root_identity.c:276` sets `02644`, driven by the tests `chown_family_keeps_virtual_root_identity_and_real_errors` (`hermit-cli/tests/chown_virtual_root_identity.rs:185`) and `chown_family_keeps_virtual_root_identity_without_a_user_namespace` (`hermit-cli/tests/chown_virtual_root_identity.rs:197`). Those tests do not select a backend.
- `tests/c/syscall_file_metadata.c:147` requires `fchmod(fd, 0600)` to return 0, cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`).

None of these calls `syscall(SYS_fchmod, ...)` directly, and none covers F2 to F7, so no coverage token is recorded.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, the kernel checks the launching host user's uid, groups and capabilities while Detcore reports uid 0, so `fchmod` on a file that user does not own fails with `EPERM` (F6) where real root would succeed, and whether `S_ISGID` is silently dropped (F2) follows the host user's groups.

## Recommended tests

- T1 (covers F1): under `hermit run --verify` on ptrace, open a fresh regular file and call raw `syscall(SYS_fchmod, fd, ...)` with modes `0640`, `0600`, `07777`, `S_IFDIR | 0644` and `0`, asserting the exact `st_mode & 07777` from `fstat` after each call, including that a second call replaces rather than ORs. Existing partial coverage via glibc `fchmod()`: `tests/c/fchmod_bits.c:52` and `tests/c/fchmod_bits.c:57` (cell `c-programs/fchmod-bits`).
- T2 (covers F2): `fchown` a file the guest created to a second mapped group the guest is not a member of, drop `CAP_FSETID` from the effective set, and assert that `fchmod(fd, 02755)` returns 0 and stores `0755`. With `CAP_FSETID` kept, assert it stores `02755`. Related existing check: `tests/c/chown_virtual_root_identity.c:127` arms `06755` and reads it back.
- T3 (covers F3): assert `EBADF` for `fd = -1`, for a closed descriptor, for a descriptor above the `RLIMIT_NOFILE` limit and for an `O_PATH` descriptor (also `O_PATH | O_NOFOLLOW` on a symlink). Assert that an `O_RDONLY` descriptor of a file the guest owns succeeds.
- T4 (covers F4): `fchmod` a descriptor after `unlink` of its only name and assert `fstat` sees the new mode; `fchmod` after `rename` and assert `stat` by the new name sees it; `fchmod` an `O_DIRECTORY` descriptor to `01777` and `02775` and assert the stored modes; `fchmod` both ends of a `pipe` and a `socketpair` and require the same return value and `fstat` mode on both runs.
- T5 (covers F5): set an access ACL with a named-user entry and a mask, `fchmod` to `0640`, and assert the ACL mask entry becomes `r--`. The fixture directory must be on a filesystem that supports ACLs; if it does not, the test fails rather than skips.
- T6 (covers F6): assert `EPERM` for a descriptor of a file owned by a uid the guest's user namespace does not map. The immutable and append-only case is F8, recorded as U1 rather than tested.
- T7 (covers F7): assert `EROFS` for a descriptor on a read-only mount the guest can see. `EIO` is F9, recorded as U2 rather than tested.
- T8 (covers F1, F3): race `fchmod(fd)` in one guest thread against `close(fd)` and `dup2(other, fd)` in another under `hermit run --verify`, and require the same outcome (errno and final mode of each inode) on both runs.
- U1 (covers F8; host-config): only the host can set the immutable or append-only flag, so CI produces this `EPERM` only on a host that flagged an inode the guest can open. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:773`, `detcore/src/lib.rs:2843`).
- U2 (covers F9; host-transient): `EIO` comes from failing host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F9 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/350#issuecomment-5872974448
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/350: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fchmod records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/350 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843, classified at detcore/src/syscall_classification.rs:773 under the https://github.com/rrnewton/hermit/pull/683 metadata group, which this body answers as holding for fchmod); listed 7 flag behaviours and 8 tests; no coverage token recorded because the run-mode cells call glibc fchmod() and cover only F1, so tracking_status stays todo; open question for the owner: under --no-namespace a guest shown uid 0 gets EPERM on inodes it does not own (fidelity, not determinism)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files); immutable or append-only EPERM split from F6 into new F8 (host-config U1); EIO split from F7 into new F9 (host-transient U2), F7 keeps EROFS; T6 and T7 narrowed; the open question on unreachable host errors is resolved by decision 1
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (EPERM on files the host user does not own, F6, and host-group S_ISGID clearing under --no-namespace and namespace-less backends); target_policy unchanged (passthrough); no new F/T ids; removed the --no-namespace fidelity open question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fchmod`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchmod&body=Discussion+for+the+%60fchmod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchmod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchmod%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fchmod" (https://github.com/rrnewton/hermit/issues/350), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/350.

The legacy body ranked `fchmod` 71st in the batch-2 classification sequence, after excluding the 50 workload-observed batch-1 syscalls. That ordering was a filing artefact and has no bearing on the policy.

### Open questions

- The new ctime is outside this issue. It belongs to the stat family tracking.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fchmod -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(91, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fchmod(-1, 0177777)             = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/350#issuecomment-5872974448
