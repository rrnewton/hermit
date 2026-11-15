---
schema: syscall-pages/v1
syscall: fanotify_mark
nr_x86_64: 301
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/349
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fanotify_mark&body=Discussion+for+the+%60fanotify_mark%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffanotify_mark.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fanotify_mark%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`fanotify_mark(fanotify_fd, flags, mask, dirfd, pathname)` adds, removes or flushes marks that tell a fanotify group which inodes, mounts or filesystems to watch and for which events. Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4). Because `fanotify_init` is refused by the same arm, a guest has no fanotify descriptor to mark in the first place.

Manual reference: [fanotify_mark(2)](https://man7.org/linux/man-pages/man2/fanotify_mark.2.html).

## Linux semantics

The kernel (`fs/notify/fanotify/fanotify_user.c`) validates `flags` and `mask` first: unknown bits, more than one of add/remove/flush, or an event bit the group's reporting mode does not allow give `EINVAL`. The host probe under Additional information, with every bit set and `fanotify_fd = -1`, stops here with `EINVAL` before looking at the descriptor. Then `fanotify_fd` must be a fanotify group (`EBADF` if not a valid descriptor, `EINVAL` if not fanotify). For an unprivileged group (one created without `CAP_SYS_ADMIN` in the initial user namespace) only inode marks are allowed; mount and filesystem marks give `EPERM`. The target object is resolved from `dirfd` and `pathname` as for `*at` calls, and the caller needs read permission on it.

On success the group's mark table for that object changes: later events on the object, by any process, are queued to the group or suppressed according to the event mask and ignore mask. Adding a mark beyond the per-group limit (`fs.fanotify.max_user_marks`) gives `ENOSPC`.

### Flags and options

- F1: `FAN_MARK_ADD`: add the bits in `mask` to the object's mark, creating it if needed.
- F2: `FAN_MARK_REMOVE`: remove the bits in `mask`; the mark goes away when its mask becomes empty; `ENOENT` if there is no mark.
- F3: `FAN_MARK_FLUSH`: remove all marks of the selected object type (inode, mount or filesystem) from the group; `pathname` is ignored.
- F4: object type: inode (default), `FAN_MARK_MOUNT`, `FAN_MARK_FILESYSTEM`, and on recent kernels `FAN_MARK_MNTNS` (mount-namespace marks for `FAN_MNT_ATTACH`/`FAN_MNT_DETACH` events). Mount and filesystem marks need a privileged group.
- F5: target resolution: `dirfd` and `pathname` (`AT_FDCWD`, or a NULL `pathname` to mark `dirfd` itself), `FAN_MARK_DONT_FOLLOW` (do not follow a final symlink), `FAN_MARK_ONLYDIR` (`ENOTDIR` if the target is not a directory).
- F6: ignore masks: `FAN_MARK_IGNORED_MASK` (legacy), `FAN_MARK_IGNORE` (Linux 6.0 and later, with stricter rules), and `FAN_MARK_IGNORED_SURV_MODIFY` (keep the ignore mask across modification).
- F7: `FAN_MARK_EVICTABLE`: an inode mark that does not pin the inode and disappears when the inode is evicted from cache.
- F8: event mask bits: `FAN_ACCESS`, `FAN_MODIFY`, `FAN_ATTRIB`, `FAN_CLOSE_WRITE`, `FAN_CLOSE_NOWRITE`, `FAN_OPEN`, `FAN_OPEN_EXEC`, `FAN_MOVED_FROM`, `FAN_MOVED_TO`, `FAN_RENAME`, `FAN_CREATE`, `FAN_DELETE`, `FAN_DELETE_SELF`, `FAN_MOVE_SELF`, `FAN_FS_ERROR`, and on recent kernels `FAN_MNT_ATTACH`/`FAN_MNT_DETACH`; plus the modifiers `FAN_ONDIR` and `FAN_EVENT_ON_CHILD`. Directory-entry events need a file-handle reporting group.
- F9: permission events: `FAN_OPEN_PERM`, `FAN_ACCESS_PERM`, `FAN_OPEN_EXEC_PERM`, and on recent kernels `FAN_PRE_ACCESS`; allowed only for a group of a permission class, and they block the accessing process until the group answers.
- F10: errors: `EINVAL` for invalid flag or mask combinations (the host probe), `EBADF` for a bad `fanotify_fd` or `dirfd`, `EPERM` for privileged object types from an unprivileged group, `ENOENT` for a missing path or mark, `ENOTDIR`, `EXDEV` for some filesystems that cannot report file handles, `ENOSPC` at the mark limit, `ENOMEM`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Changing a mark is itself a deterministic table update for fixed inputs, but it controls the delivery of events that every process on the host generates asynchronously. What the group later reads depends on concurrent filesystem activity, on the lifetime of inodes and mounts (evictable marks vanish with cache pressure), and on the group's queue limits. Permission events add a second effect: the marked object's accessors block until the group responds, so one process's timing changes another's.

### Nondeterminism sources

- Concurrent filesystem events from any process touching the marked object
- Mount and inode lifecycle, including cache eviction that removes `FAN_MARK_EVICTABLE` marks
- Fanotify group state: its class, reporting mode, queue and mark limits, and pending permission decisions
- Host privilege and kernel version, which decide which object types and event bits are accepted

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `fanotify_mark` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724), which names global filesystem-event notification among the state the container pins.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm reads no argument and is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership and runs no guest.

Why `refuse`:

- `fanotify_init` is refused by the same arm, so under Hermit no descriptor can be a fanotify group. Any `fanotify_mark` from the guest is acting on a descriptor that is not a fanotify group, which Linux would also reject (`EBADF` or `EINVAL`). Refusing keeps the pair consistent.
- A working mark would feed the guest an asynchronous event stream from the host, which Detcore does not model.
- If `fanotify_init` ever moves to `model`, this call must move with it; the two policies should change together.

Known difference from Linux: Hermit returns `EPERM` where Linux returns `EINVAL` (the host probe), `EBADF` or `ENOENT`. A program that probes for fanotify by calling `fanotify_mark` on a bad descriptor sees `EPERM` instead of `EBADF`.

Target (owner decision 6): keep the fixed `EPERM`, returned before any argument is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's order is `do_fanotify_mark` in `fs/notify/fanotify/fanotify_user.c`: unknown `flags` bits, an invalid mark type or command, and invalid `mask` bits give `EINVAL`; then `fanotify_fd` is looked up (`EBADF` if it is not open, `EINVAL` if it is not a fanotify group); only then come the `CAP_SYS_ADMIN` check for mount and filesystem marks (`EPERM`) and the path lookup (`ENOENT`). Hermit deliberately does not reproduce that order. Because `fanotify_init` fails first, a guest reaches this call only with a descriptor that is not a fanotify group; programs that fall back from fanotify to inotify do so on the `fanotify_init` failure.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7): under `hermit run --verify` on ptrace, a guest calls `syscall(SYS_fanotify_mark, ...)` on an ordinary file descriptor (since `fanotify_init` fails) with `FAN_MARK_ADD`, `FAN_MARK_REMOVE`, `FAN_MARK_FLUSH`, `FAN_MARK_ADD|FAN_MARK_MOUNT`, `FAN_MARK_ADD|FAN_MARK_FILESYSTEM`, `FAN_MARK_ADD|FAN_MARK_DONT_FOLLOW|FAN_MARK_ONLYDIR`, `FAN_MARK_ADD|FAN_MARK_IGNORE`, and `FAN_MARK_ADD|FAN_MARK_EVICTABLE`, each with `AT_FDCWD` and a path it created. Each must return -1 with `EPERM`, and both runs must agree.
- T2 (covers F8, F9, F10): the same guest uses masks with every event bit, with each permission bit, and the host probe's all-bits arguments with `fanotify_fd = -1`. Each returns the fixed `EPERM` of owner decision 6 (Linux gives `EINVAL` for the probe and `EBADF` or `EINVAL` otherwise), and both `--verify` runs agree.
- T3 (covers F1, F8): `hermit record` then `hermit replay` a guest that tries `fanotify_init` and, if that fails, `fanotify_mark` on `fd = -1`, printing each result; recording and replay must match.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/349#issuecomment-5872971952
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/349: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fanotify_mark records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/349 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every fanotify_mark request, never forwarded; fanotify_init is refused by the same arm, so no guest descriptor exists to mark); linux_determinism stays nondeterministic; 10 flag behaviours and 3 tests listed; no coverage token because no test runs a guest that calls fanotify_mark, so tracking_status stays todo; open question on errno order under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before any argument check stays the target; Linux order in do_fanotify_mark deliberately not reproduced); target_policy stays refuse; T2 now asserts the decision-6 errno; no new F/T ids; open question on checking flags, mask and descriptor first removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fanotify_mark`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fanotify_mark&body=Discussion+for+the+%60fanotify_mark%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffanotify_mark.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fanotify_mark%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fanotify_mark" (https://github.com/rrnewton/hermit/issues/349), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/349.

The legacy body ranked it 70th in the batch-2 classification sequence, after excluding the 50 workload-observed batch-1 syscalls. It pairs with `fanotify_init` (https://github.com/rrnewton/hermit/issues/348).

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fanotify_mark -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(301, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fanotify_mark(-1, FAN_MARK_ADD|FAN_MARK_REMOVE|FAN_MARK_DONT_FOLLOW|FAN_MARK_ONLYDIR|FAN_MARK_MOUNT|FAN_MARK_IGNORED_MASK|FAN_MARK_IGNORED_SURV_MODIFY|FAN_MARK_FLUSH|FAN_MARK_FILESYSTEM|FAN_MARK_EVICTABLE|FAN_MARK_IGNORE|0xfffff800, FAN_ACCESS|FAN_MODIFY|FAN_ATTRIB|FAN_CLOSE_WRITE|FAN_CLOSE_NOWRITE|FAN_OPEN|FAN_MOVED_FROM|FAN_MOVED_TO|FAN_CREATE|FAN_DELETE|FAN_DELETE_SELF|FAN_MOVE_SELF|FAN_OPEN_EXEC|FAN_Q_OVERFLOW|FAN_FS_ERROR|FAN_OPEN_PERM|FAN_ACCESS_PERM|FAN_OPEN_EXEC_PERM|FAN_DIR_MODIFY|FAN_ONDIR|FAN_EVENT_ON_CHILD|FAN_RENAME|0xffffffffa7f02000, FAN_NOFD, 0xffffffffffffffff) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/349#issuecomment-5872971952
