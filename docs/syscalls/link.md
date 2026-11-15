---
schema: syscall-pages/v1
syscall: link
nr_x86_64: 86
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/630
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+link&body=Discussion+for+the+%60link%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60link%60%2C+or+in+what+the+page+says%3F%0A
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

`link(oldpath, newpath)` creates a new directory entry `newpath` for the existing inode named by `oldpath` (a hard link). Hermit forwards it to the host kernel unchanged. The result repeats under Hermit's private, stable container filesystem with one exception: the host-wide `fs.protected_hardlinks` setting decides whether some links are allowed.

## Linux semantics

The kernel resolves `oldpath` without following a symlink in its last component, and resolves the parent of `newpath`, following symlinks in every earlier component of both. Both resolve relative to the caller's working directory. `newpath` must not exist and must be on the same mount as `oldpath`. On success both names refer to one inode, whose link count goes up by one and whose ctime changes; the new parent's mtime and ctime change too. The call returns 0 or a negative errno and writes nothing to guest memory. Before linking, the kernel's `may_linkat` check applies the `fs.protected_hardlinks` sysctl: when it is 1, a caller that does not own the inode (and lacks `CAP_FOWNER` over its owner) may link it only if it is a regular file that is not setuid, not setgid-executable, and is readable and writable by the caller. Errors are otherwise checked in path-walk order, so the errno is a function of the two paths, the filesystem state, the credentials, the mounts and that one sysctl.

### Flags and options

- F1: `newpath` does not exist: a second name for the inode is created, `st_nlink` goes up by one, and both names show the same `st_ino`
- F2: `oldpath` is a symlink: `link()` links the symlink itself, not its target (Linux does not follow it; POSIX allows either)
- F3: `newpath` already exists as any type, including a dangling symlink: `EEXIST`
- F4: `oldpath` is a directory: `EPERM`
- F5: `oldpath` and `newpath` are on different mounts, including two bind mounts of one filesystem: `EXDEV`
- F6: `fs.protected_hardlinks` is 1 and the caller neither owns the inode nor has `CAP_FOWNER` over its owner, and the inode is not a regular file that the caller can read and write (or it is setuid, or setgid and group-executable): `EPERM`. With the sysctl at 0 the same call succeeds.
- F7: trailing slash: on `oldpath` naming a non-directory: `ENOTDIR`. On an absent `newpath`: `ENOENT`.
- F8: `oldpath` is missing, either path is empty, or `newpath`'s parent is missing: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F9: write permission is denied on `newpath`'s parent, or search permission on a path component: `EACCES`.
- F10: a component is longer than `NAME_MAX`, or a path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in a prefix: `ELOOP`. Either path points outside the address space: `EFAULT`.
- F11: `newpath` lies under a read-only mount the guest can see: `EROFS`
- F12: a relative path in either argument resolves against the calling process's working directory, inside its root and mount namespace
- F13: host-config failure: the inode is immutable or append-only: `EPERM`. Setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks, so only files the host flagged give it.
- F14: host-config failures: `EMLINK` when the inode's link count is at the limit of the host filesystem type (65000 on ext4, far higher on XFS or tmpfs), and `EOVERFLOW` on an idmapped host mount on which the inode's owner has no mapping
- F15: host-transient failures: `ENOSPC` (the directory needs a new block), `EDQUOT` (quota) and `EIO`
- F16: host-config failure: the host filesystem holding the file does not support hard links: `EPERM`

## Determinism analysis

`conditional`. The result is a function of the filesystem state along both paths, the caller's credentials, the mount layout, and the host-wide `fs.protected_hardlinks` sysctl (F6). Nothing in the call depends on timing, and Hermit runs threads one at a time, so two guest threads creating the same name get a fixed order. The first four inputs are container state that Hermit holds fixed from run to run on one host; the mount layout can still differ between hosts (Cross-host variation, F5). The sysctl is not fixed even on one image: it belongs to the host, not to the container's filesystem image, so the same guest on the same image can get 0 on one host and `EPERM` on another. Under ptrace the guest is root in a user namespace, so files owned by host uids outside the mapping (for example, root-owned files from the launch image) are exactly the ones F6 applies to. On one host with an unchanged sysctl, the call repeats exactly.

### Nondeterminism sources

- The host's `fs.protected_hardlinks` setting (F6), which differs between hosts and can be changed by the host administrator between runs.
- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F13 and F14: inode flags, filesystem link limits, idmapped mounts. Host resources behind F15: free space, quota, storage errors.
- Values the call changes but does not return: the inode's link count and ctime, and the parent's timestamps. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- `fs.protected_hardlinks` sysctl: the same link of a file owned by an unmapped uid gives `EPERM` on a host with the sysctl at 1 and succeeds on a host with it at 0 (F6).
- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F13).
- Filesystem type and mount setup under the guest's files: the link-count limit decides when `EMLINK` appears, and an idmapped host mount can give `EOVERFLOW` (F14).
- Mount layout the guest sees: two paths on one filesystem on one host can be on different mounts on another, so the same link succeeds on one and gives `EXDEV` on the other (F5). A host filesystem without hard-link support gives `EPERM` (F16).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `link` is classified `PassThrough` at `detcore/src/syscall_classification.rs:785`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. Its group comment at `detcore/src/syscall_classification.rs:767` and `:768` says these calls "are repeatable in Hermit's fixed mount namespace and filesystem image", and `:769` leaves the boundary open for human review. The comment at `detcore/src/syscall_classification.rs:725` and `:726`, which covers `linkat` (`detcore/src/syscall_classification.rs:729`), gives the same condition.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::link` at `detcore/src/syscall_classification.rs:2071` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F12 the same way the guest's other path syscalls see them.
- Emulating the call would not remove the F6 condition. Hermit would have to pick a `protected_hardlinks` value and then fake the link or the refusal, which would disagree with the host kernel that serves every other path call on the same files.
- The return value carries no host-specific data: 0 or an errno that follows from the inputs. The one host input, F6, is a condition on where a recording or a verify run is compared, which is why `linux_determinism` is `conditional` while `target_policy` stays `passthrough`. Whether Hermit should refuse to start, or warn, when that sysctl differs from a recorded value is an open question below.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `link`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `link`. The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires raw `SYS_link` calls with relative paths to replay, for example `tests/c/record_replay_path_queries.c:126`, `:147`, `:190` and `:239`. The `st_nlink != 2` check at `tests/c/record_replay_path_queries.c:251` reads back the link made at `:239`.

Existing run-mode evidence: the cargo integration test `deterministic_file_metadata_syscalls_verify` (`hermit-cli/tests/syscall_file_metadata.rs:51`) runs `tests/c/syscall_file_metadata.c` under `--backend=ptrace run --strict` and then `--verify`. That guest calls glibc `link(path, hardlink_path)` at `tests/c/syscall_file_metadata.c:159` with absolute paths and requires 0. On this host's glibc 2.34, glibc `link()` issues `SYS_link` (host strace under Additional information). The test's list of syscalls that must appear in the trace (`hermit-cli/tests/syscall_file_metadata.rs:91` to `:99`) does not include `link`, so it does not prove the raw syscall was seen.

No coverage token is recorded yet: these tests cover only F1, and the remaining behaviours have no test.

## Recommended tests

- T1 (covers F1, F12): under `hermit run --verify` on ptrace, `chdir` into a fresh directory and call raw `syscall(SYS_link, ...)` with relative paths. Assert that both names show the same `st_ino` and that `st_nlink` goes from 1 to 2, and that writing through one name is visible through the other.
  Existing partial coverage: `hermit-cli/tests/syscall_file_metadata.rs:51` (glibc `link()` at `tests/c/syscall_file_metadata.c:159`, absolute paths, success only).
- T2 (covers F2, F3): link a symlink and a dangling symlink and check with `lstat` that the new name is a symlink with the same `st_ino`. Assert `EEXIST` for an existing file, an existing directory and a dangling symlink at `newpath`.
- T3 (covers F4, F5): assert `EPERM` for a directory `oldpath`. Assert `EXDEV` between two distinct guest-visible mounts. Record the chosen paths in the test.
- T4 (covers F6): read `/proc/sys/fs/protected_hardlinks` and print it. Link a file owned by an unmapped host uid that the guest cannot both read and write, on the same mount as a writable directory. Assert `EPERM` when the sysctl is 1 and 0 when it is 0, and print the outcome so that a verify run on a host with a different value shows the difference.
- T5 (covers F7, F8): assert `ENOTDIR` for `link("f/", "g")` and `ENOENT` for `link("f", "g/")` with `g` absent. Assert `ENOENT` for a missing `oldpath`, for `""` in either argument and for a missing parent of `newpath`, and `ENOTDIR` when a regular file is used as an intermediate component.
- T6 (covers F9): assert `EACCES` when `newpath`'s parent is a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid.
  The immutable or append-only case is F13, recorded as U2; this test does not cover it.
- T7 (covers F10): assert `ENAMETOOLONG` for a 256-byte component and for a path over 4096 bytes in each argument, `ELOOP` for a symlink loop in a prefix, and `EFAULT` for `oldpath = (char *)1` and for `newpath = (char *)1`.
- T8 (covers F11): assert `EROFS` for a link under a read-only mount the guest can see. `EMLINK` and `EOVERFLOW` (F14) are recorded as U2, and `ENOSPC`, `EDQUOT` and `EIO` (F15) as U3; this test does not provoke them.
- U1 (covers F6; host-config): `fs.protected_hardlinks` is a host sysctl the guest cannot set, so one CI host exercises only one of its two answers; T4 records which. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:785`, `detcore/src/lib.rs:2843`).
- U2 (covers F5, F13, F14, F16; host-config): the mount layout the guest sees, immutable and append-only flags, the filesystem's link-count limit and hard-link support, and idmapped mounts belong to the host, so which paths give `EXDEV`, and whether the other errors appear at all, depends on the CI host; T3 still checks `EXDEV` for one pair of distinct mounts. Hermit forwards them unchanged (`detcore/src/syscall_classification.rs:785`, `detcore/src/lib.rs:2843`).
- U3 (covers F15; host-transient): `ENOSPC`, `EDQUOT` and `EIO` come from host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F15 counts toward done only once Hermit hides these errors or reports them as infrastructure errors.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/630: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (the code forwards at detcore/src/lib.rs:2843; the condition is the host-wide fs.protected_hardlinks sysctl, F6, which is outside the container image); listed 12 flag behaviours and 8 tests; existing ptrace evidence (hermit-cli/tests/syscall_file_metadata.rs, glibc link) covers only F1, so no coverage token is recorded and tracking_status stays todo; open question for the owner on pinning fs.protected_hardlinks
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (fs.protected_hardlinks sysctl, mount layout, inode flags on host files, filesystem link limit and idmapped mounts); host-config U1 for F6; EXDEV (F5) added to host-config U2; hard-link-support EPERM split from F4 as new F16 (host-config U2); immutable EPERM split from F9 as new F13; F11 narrowed to EROFS with EMLINK and EOVERFLOW split out as new F14 (host-config U2) and ENOSPC, EDQUOT and EIO as new F15 (host-transient U3); T6 and T8 narrowed; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `link`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+link&body=Discussion+for+the+%60link%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60link%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: link" (https://github.com/rrnewton/hermit/issues/630), legacy variant B.

Legacy text replaced in this pass: Summary "Audit the x86_64 `link(2)` syscall and replace its temporary catch-all policy with an explicit Detcore classification." (done: `link` has an explicit pass-through arm); Determinism analysis "Repeats under stable isolated filesystem state, matching the reviewed linkat policy." That claim is kept above, with the `fs.protected_hardlinks` condition that makes it `conditional` rather than `deterministic`.

### Open questions

- F6: should Hermit treat `fs.protected_hardlinks` as part of a run's environment, for example by recording it and refusing or warning on replay or verify when it differs, or is a documented host precondition enough? This decides whether `linux_determinism` can ever move from `conditional` under Hermit. The same sysctl family (`fs.protected_symlinks`, `fs.protected_regular`, `fs.protected_fifos`) affects other path calls.

### 2026-10-03 host strace evidence

On this host (glibc 2.34, x86_64), a scratch probe calling glibc `link()` under `strace -f` showed the raw syscall, so the glibc-based test above does exercise `SYS_link`:

```
link("b", "c")                          = 0
```

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
