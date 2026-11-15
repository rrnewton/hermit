---
schema: syscall-pages/v1
syscall: mknod
nr_x86_64: 133
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/288
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mknod&body=Discussion+for+the+%60mknod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmknod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mknod%60%2C+or+in+what+the+page+says%3F%0A
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

`mknod(pathname, mode, dev)` creates one filesystem node at `pathname`: a regular file, a FIFO, a Unix-socket inode, or (with privilege) a character or block device. It is the legacy form of `mknodat(AT_FDCWD, ...)`. Detcore passes it straight to the kernel: the generic arm at `detcore/src/lib.rs:2843` forwards it unchanged.

## Linux semantics

The kernel resolves `pathname` relative to the caller's working directory and root, takes the file type from `mode & S_IFMT` and the permission bits from `mode & 07777`, applies the umask (or the parent directory's default ACL), and creates the node. The call returns 0 or an errno; it creates nothing on failure and returns no inode or descriptor. `dev` is used only for character and block devices.

Device nodes need `CAP_MKNOD` in the initial user namespace. Root inside an unprivileged user namespace, which is what a container usually has, gets `EPERM`. The one exception is a character device with device number 0:0, the overlayfs "whiteout", which any user may create.

Host results measured for this issue on Linux 7.1.3 as an unprivileged user: type 0, `S_IFREG`, `S_IFIFO` and `S_IFSOCK` succeed; `S_IFCHR` and `S_IFBLK` give `EPERM` except `S_IFCHR` with `dev == 0`, which succeeds; `S_IFDIR` gives `EPERM`; `S_IFLNK` and `0170000` give `EINVAL`; an existing path gives `EEXIST`; mode 0777 under umask 077 creates 0700; a FIFO created with a nonzero `dev` has `st_rdev == 0`.

### Flags and options

- F1: file type 0 or `S_IFREG`: creates an empty regular file
- F2: `S_IFIFO`: creates a named pipe; `dev` is ignored and the node's `st_rdev` is 0
- F3: `S_IFSOCK`: creates a socket inode that no socket is bound to
- F4: `S_IFCHR` or `S_IFBLK` with a nonzero `dev`: creates a device node with that major and minor only with `CAP_MKNOD` in the initial user namespace; otherwise `EPERM`
- F5: `S_IFCHR` with `dev == 0` (whiteout): created without `CAP_MKNOD`
- F6: `S_IFDIR` returns `EPERM`; `S_IFLNK` and any other invalid type (for example `0170000`) return `EINVAL`
- F7: permission bits: `mode & 07777` filtered through the umask, or through the parent's default ACL when it has one; setuid, setgid and sticky bits follow the filesystem's rules
- F8: `pathname` already exists (including a dangling symlink): `EEXIST`, nothing changes
- F9: path errors: `ENOENT` (missing parent or empty path), `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, `EACCES` (no write or search permission), `EFAULT` (bad pointer)
- F10: filesystem errors: `EROFS`, `ENOSPC`, `EDQUOT`, and `EPERM` from filesystems that do not support the node type
- F11: relative `pathname` resolves against the calling thread's working directory and root

## Determinism analysis

`linux_determinism: deterministic`. The result is a function of the arguments, the caller's credentials and umask, and the state of the directory it writes into. The container identity is fixed at start (`detcore/src/syscall_classification.rs:714`), and so is the container's file tree, so two runs that reach the call with the same tree get the same result. The created node's inode number and timestamps are read later through `stat`, not returned here; they belong to `stat` and the filesystem.

### Nondeterminism sources

- None from the call itself; `dev` and `mode` come from the caller.
- Host filesystem state outside the guest's control (free space for `ENOSPC`, quotas for `EDQUOT`, a read-only mount for `EROFS`) changes the result only if the host changes between runs, which is outside Hermit's model.
- `CAP_MKNOD` in the initial namespace (F4) is a property of how Hermit is launched; it is the same for every run from the same launch configuration.

## Hermit design

Target policy: `passthrough`. This matches the code at hermit f4055eb9cae4.

- `classify_syscall` returns `PassThrough` for `mknod` in the "ISSUE-REVIEWED PASS-THROUGH SYSCALLS" block (`detcore/src/syscall_classification.rs:814`). The block's comment at `detcore/src/syscall_classification.rs:808` says these remain conditional on Hermit's fixed-container and stable-state assumptions, and `detcore/src/syscall_classification.rs:811` still carries `TODO-HUMAN-REVIEW(PR-643)`.
- The unit test `representative_policies_stay_in_their_reviewed_sections` checks that classification (`detcore/src/syscall_classification.rs:2103`).
- Dispatch is the generic arm at `detcore/src/lib.rs:2843`, which calls `passthrough` (`detcore/src/lib.rs:367`), which forwards through `record_or_replay_preserving_tool_errors`. In `hermit run` the call is intercepted (so logical time is charged) and forwarded unchanged.
- Under `hermit record` and `hermit replay`, `passthru_opt` is on (`detcore/src/lib.rs:3140`), and Detcore does not subscribe pass-through syscalls (`detcore/src/lib.rs:1084`; unit test `passthru_opt_leaves_unlisted_passthrough_syscalls_unsubscribed` at `detcore/src/lib.rs:3185`).

Why not `model`: nothing the call returns depends on host state that Detcore hides elsewhere. The node becomes part of the guest's file tree, which Hermit already treats as guest-owned. Device nodes fail with `EPERM` inside the container without any help from Detcore.

### Gaps and open points

- No ptrace test reaches `SYS_mknod` directly. `tests/c/mknod_special.c` uses glibc `mkfifo`, `mknodat` and `mknod`; on x86_64 glibc's `mknod` wrapper is expected to issue `mknodat`, not `mknod` (inferred, not traced here), so that test probably exercises `mknodat`.
- `tests/c/record_replay_path_queries.c:243` calls `syscall(SYS_mknod, "p", S_IFIFO | 0600, 0)`, but it is driven only by the record/replay test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2170`), not by a `hermit run` ptrace cell.

## Recommended tests

- T1 (covers F1, F2, F3, F7): raw `syscall(SYS_mknod, ...)` under `hermit run --verify` on ptrace with type 0, `S_IFREG`, `S_IFIFO` (with a nonzero `dev`) and `S_IFSOCK`, under umask 022 and 077. `lstat` each node and print the file type, permission bits and `st_rdev`.
  Existing candidate: `tests/c/mknod_special.c` (cell `c-programs/mknod-special`, `tests/e2e/manifests/c-programs.yaml:12642`, verify ci on ptrace) covers FIFO and regular nodes through glibc, which probably reaches `mknodat`, not `mknod`.
- T2 (covers F4, F5, F6): raw `SYS_mknod` with `S_IFCHR` 1:3, `S_IFBLK` 7:0, `S_IFCHR` 0:0, `S_IFDIR`, `S_IFLNK` and `0170000`. Expect `EPERM`, `EPERM`, success, `EPERM`, `EINVAL`, `EINVAL` inside the container, identical across runs.
- T3 (covers F8, F9): `EEXIST` on an existing file and on a dangling symlink; `ENOENT` for a missing parent and for `""`; `ENOTDIR` through a regular file; `ENAMETOOLONG`; `ELOOP` through a symlink loop; `EFAULT` for a bad pointer.
  Existing candidate for `EEXIST`: `tests/c/mknod_special.c`, through glibc.
- T4 (covers F10): `mknod` on a read-only filesystem gives `EROFS`, and on a filesystem that rejects the node type gives `EPERM`; each result is identical across runs.
- T5 (covers F11): `chdir` into a subdirectory, then `mknod("rel", S_IFIFO | 0600, 0)`; the node appears in that subdirectory. `tests/c/record_replay_path_queries.c:243` does this under record and replay only.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/288#issuecomment-5872835168
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/288: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mknod records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/288 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic and target_policy passthrough kept (PassThrough at syscall_classification.rs:814; the result depends only on the arguments, the fixed container identity and the guest's file tree); 11 flag behaviours and 5 tests; no ptrace run test reaches SYS_mknod directly (mknod_special.c goes through glibc, record_replay_path_queries.c runs only under record/replay)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mknod`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mknod&body=Discussion+for+the+%60mknod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmknod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mknod%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mknod" (https://github.com/rrnewton/hermit/issues/288), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/288.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mknod`, and by confirming hermit currently routes `mknod` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mknod("/tmp/sysclass/hf", S_IFREG|0644) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/288#issuecomment-5872835168
