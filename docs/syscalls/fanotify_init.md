---
schema: syscall-pages/v1
syscall: fanotify_init
nr_x86_64: 300
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/348
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fanotify_init&body=Discussion+for+the+%60fanotify_init%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffanotify_init.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fanotify_init%60%2C+or+in+what+the+page+says%3F%0A
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

`fanotify_init(flags, event_f_flags)` creates a fanotify notification group and returns a file descriptor from which filesystem events (and, for permission classes, access decisions) are read. Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4), so a guest never obtains a fanotify descriptor and `fanotify_mark` has nothing to act on.

Manual reference: [fanotify_init(2)](https://man7.org/linux/man-pages/man2/fanotify_init.2.html).

## Linux semantics

The kernel (`fs/notify/fanotify/fanotify_user.c`) first checks privilege. `CAP_SYS_ADMIN` is checked with `capable()`, that is, in the initial user namespace. Without it (since Linux 5.13) only a limited group is allowed: notification class only, events reported with file handles (one of the `FAN_REPORT_*FID` modes), no unlimited queue or marks and no `FAN_REPORT_TID`; anything else gives `EPERM`. The host probe under Additional information, with every bit set, stops here with `EPERM`. A process that is root only inside a user namespace, as Hermit's ptrace guest is (`detcore/src/syscalls/misc.rs:672`), counts as unprivileged.

Then the flags are validated (`EINVAL` for unknown bits, an invalid class, or inconsistent reporting flags), a group is allocated with its event queue and limits, and an anonymous-inode descriptor is installed in the lowest free slot. The group sees nothing until `fanotify_mark` adds marks; events then arrive asynchronously from any process touching the marked objects.

### Flags and options

- F1: notification class: `FAN_CLASS_NOTIF` (0), `FAN_CLASS_CONTENT` or `FAN_CLASS_PRE_CONTENT` (the two permission classes need `CAP_SYS_ADMIN`); the value `0xc` is not a class and gives `EINVAL` (with privilege) or `EPERM` (without).
- F2: descriptor flags `FAN_CLOEXEC` and `FAN_NONBLOCK` on the group descriptor.
- F3: `FAN_UNLIMITED_QUEUE` and `FAN_UNLIMITED_MARKS`: lift the default queue (16384 events) and mark limits; need `CAP_SYS_ADMIN`.
- F4: `FAN_ENABLE_AUDIT`: let permission responses write audit records; needs `CAP_AUDIT_WRITE`.
- F5: file-handle reporting: `FAN_REPORT_FID`, `FAN_REPORT_DIR_FID`, `FAN_REPORT_NAME`, `FAN_REPORT_TARGET_FID` (with their consistency rules, `EINVAL` otherwise), and on recent kernels `FAN_REPORT_FD_ERROR` and `FAN_REPORT_MNT`. A file-handle mode is required for an unprivileged group.
- F6: process reporting: `FAN_REPORT_TID` (thread ID instead of process ID; needs `CAP_SYS_ADMIN`) and `FAN_REPORT_PIDFD` (a pidfd for the event's process; not combinable with `FAN_REPORT_TID`).
- F7: `event_f_flags`: open flags for the descriptors that events carry in non-file-handle groups (`O_RDONLY`, `O_WRONLY`, `O_RDWR`, `O_LARGEFILE`, `O_CLOEXEC`, `O_APPEND`, `O_DSYNC`, `O_NOATIME`, `O_NONBLOCK`, `O_SYNC`); other bits give `EINVAL`.
- F8: privilege: `CAP_SYS_ADMIN` in the initial user namespace for everything; without it, only `FAN_CLASS_NOTIF` with a file-handle mode and no F3 or `FAN_REPORT_TID` bits, otherwise `EPERM` (the host probe).
- F9: other errors: `EMFILE` at the per-user group limit (`fs.fanotify.max_user_groups`) or the descriptor limit, `ENOMEM`, `ENOSYS` without `CONFIG_FANOTIFY`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. The call itself repeats for fixed inputs and privilege, but its purpose is to subscribe to an asynchronous stream of host filesystem activity. Once marks exist, which events arrive, in what order, how many are merged or dropped on overflow, and which process IDs, descriptors and file handles they carry depend on every process on the host touching the marked objects. Whether the call succeeds depends on host privilege, kernel configuration and the per-user group limit.

### Nondeterminism sources

- Descriptor allocation and per-user group bookkeeping (`fs.fanotify.max_user_groups`)
- The host filesystem event stream: timing, ordering, merging and overflow of events from all processes
- Host-dependent event contents: process IDs, file handles (which embed host inode numbers and filesystem IDs), and event descriptors
- Privilege, kernel version (which reporting flags exist) and kernel configuration

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `fanotify_init` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724). The comment there names "configure global filesystem-event notification" among the operations a deterministic container pins.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm reads neither argument and is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership and runs no guest.

Why `refuse`:

- Events are produced asynchronously by the host kernel and carry host identities (process IDs, inode-based file handles). Detcore would have to generate the event stream itself from the guest's own file operations, in a deterministic order, to `model` it; nothing at this SHA does.
- `EPERM` is what Linux returns to the guest for the privileged modes, since `CAP_SYS_ADMIN` is checked in the initial user namespace.
- Refusing `fanotify_init` makes `fanotify_mark` unreachable with a real descriptor, which keeps the pair consistent.

Known differences from Linux:

- On Linux 5.13 and later an unprivileged process, including the Hermit guest, can create a `FAN_CLASS_NOTIF` group with `FAN_REPORT_FID` and watch inodes it can read. Under Hermit that fails with `EPERM`. Programs that fall back to inotify when fanotify is unavailable keep working; Detcore has its own inotify handlers (`detcore/src/lib.rs:2581-2587`).
- Hermit returns `EPERM` for invalid flags where a privileged Linux caller would get `EINVAL`.

Target (owner decisions 6 and 9): keep the fixed `EPERM` for every request, which is what the code does today (`detcore/src/lib.rs:2215-2219`). The unprivileged subset is not modelled: decision 9 models a refused family only where a named workload breaks under refusal, and no workload in the tree is known to need fanotify. The refusal matches a configuration Linux itself presents: before Linux 5.13 every unprivileged `fanotify_init` returns `EPERM`, so a program that supports those kernels already meets that error, and file watchers fall back to inotify, which Detcore handles (`detcore/src/lib.rs:2581-2587`).

## Recommended tests

- T1 (covers F1, F2, F5, F6, F7): under `hermit run --verify` on ptrace, a guest calls `syscall(SYS_fanotify_init, ...)` with the unprivileged subset `FAN_CLASS_NOTIF|FAN_REPORT_FID|FAN_CLOEXEC|FAN_NONBLOCK` and `O_RDONLY`, with `FAN_REPORT_DFID_NAME|FAN_REPORT_PIDFD`, and with `FAN_CLASS_CONTENT` and `O_RDWR|O_LARGEFILE`. Each must return -1 with `EPERM`, and the next `open` must get the same descriptor number it would have got without the calls.
- T2 (covers F3, F4, F8, F9): the same guest tries `FAN_UNLIMITED_QUEUE|FAN_UNLIMITED_MARKS`, `FAN_ENABLE_AUDIT`, `FAN_REPORT_TID`, and the host probe's all-bits value. Each returns the fixed `EPERM` of owner decision 6 (the host probe also gave `EPERM`), and both `--verify` runs agree.
- T3 (covers F1, F5): `hermit record` then `hermit replay` a guest that tries `fanotify_init`, falls back to `inotify_init1` on failure, and prints which path it took; the recording and replay must both report the fallback.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/348#issuecomment-5872969203
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/348: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fanotify_init records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/348 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every fanotify_init request, never forwarded, so the guest never holds a fanotify descriptor); linux_determinism stays nondeterministic; 9 flag behaviours and 3 tests listed; no coverage token because no test runs a guest that calls fanotify_init, so tracking_status stays todo; open question on modelling the unprivileged subset under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 9 applied (fixed EPERM stays the target; the unprivileged subset is not modelled because no named workload needs it, and EPERM is what Linux before 5.13 gives every unprivileged caller); target_policy stays refuse; T2 now asserts the decision-6 errno; no new F/T ids; open question on modelling the unprivileged subset removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fanotify_init`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fanotify_init&body=Discussion+for+the+%60fanotify_init%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffanotify_init.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fanotify_init%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fanotify_init" (https://github.com/rrnewton/hermit/issues/348), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/348.

The legacy body ranked it 69th in the batch-2 classification sequence, after excluding the 50 workload-observed batch-1 syscalls.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fanotify_init -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(300, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fanotify_init(0xc /* FAN_CLASS_??? */|FAN_CLOEXEC|FAN_NONBLOCK|FAN_UNLIMITED_QUEUE|FAN_UNLIMITED_MARKS|FAN_ENABLE_AUDIT|FAN_REPORT_PIDFD|FAN_REPORT_TID|FAN_REPORT_FID|FAN_REPORT_DIR_FID|FAN_REPORT_NAME|FAN_REPORT_TARGET_FID|0xffffe000, O_ACCMODE|O_CREAT|O_EXCL|O_NOCTTY|O_TRUNC|O_APPEND|O_NONBLOCK|O_SYNC|O_DIRECT|O_LARGEFILE|O_NOFOLLOW|O_NOATIME|O_CLOEXEC|O_PATH|O_TMPFILE|FASYNC|0xff80003c) = -1 EPERM (Operation not permitted)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/348#issuecomment-5872969203
