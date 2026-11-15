---
schema: syscall-pages/v1
syscall: open_by_handle_at
nr_x86_64: 304
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/313
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+open_by_handle_at&body=Discussion+for+the+%60open_by_handle_at%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopen_by_handle_at.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60open_by_handle_at%60%2C+or+in+what+the+page+says%3F%0A
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

`open_by_handle_at(mount_fd, handle, flags)` opens the file named by a `struct file_handle` that `name_to_handle_at` produced, and returns a new file descriptor. Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4). This matches the companion call: `name_to_handle_at` fails with `EOPNOTSUPP` under Hermit (`detcore/src/lib.rs:2593-2595`), so a guest cannot obtain a handle to open.

Manual reference: [open_by_handle_at(2)](https://man7.org/linux/man-pages/man2/open_by_handle_at.2.html).

## Linux semantics

The kernel (`fs/fhandle.c`) resolves `mount_fd` first: it names the mount on which the handle is decoded (`AT_FDCWD` means the current directory's mount), and a bad descriptor gives `EBADF`, as the host probe under Additional information shows with `mount_fd = -1`. The caller then needs `CAP_DAC_READ_SEARCH`, otherwise `EPERM`; Linux 6.10 relaxed this so that, in limited cases, a caller holding the capability over the filesystem's user namespace and able to reach the mount may also open handles. Hermit's ptrace guest is root only of its own user namespace (`detcore/src/syscalls/misc.rs:672`), so on the host it lacks the capability for the filesystems it sees.

The handle is copied in (`EFAULT` on a bad pointer); `handle_bytes` must be between 1 and `MAX_HANDLE_SZ` (128), otherwise `EINVAL`. The filesystem decodes `handle_type` and the opaque bytes into an inode; a handle for a deleted file or one the filesystem does not recognise gives `ESTALE`. Then the file is opened with `flags` as for `open(2)`, bypassing path lookup and directory permission checks entirely, which is why the call is privileged.

The handle bytes encode host-internal identities such as inode and generation numbers (the probe's handle has type 77 and 20 bytes), so they are not portable between filesystems, hosts or, for some filesystems, reboots.

### Flags and options

- F1: `mount_fd`: a descriptor for any file on the filesystem the handle belongs to, or `AT_FDCWD`; a bad descriptor gives `EBADF` (the host probe).
- F2: `handle`: `handle_bytes` within 1..128 (else `EINVAL`), `handle_type` and opaque `f_handle` bytes from `name_to_handle_at`; an unknown or stale handle gives `ESTALE`; a bad pointer gives `EFAULT`.
- F3: access mode and status flags in `flags` as for `open(2)`: `O_RDONLY`, `O_WRONLY`, `O_RDWR`, `O_APPEND`, `O_NONBLOCK`, `O_SYNC`, `O_DSYNC`, `O_NOATIME`, `O_TRUNC`, `O_LARGEFILE`, `O_DIRECT`; opening a directory for writing gives `EISDIR`. Path-creation flags (`O_CREAT`, `O_EXCL`) have no path to act on.
- F4: `O_CLOEXEC`: close-on-exec on the returned descriptor.
- F5: `O_PATH`: required when the handle refers to a symbolic link (otherwise `ELOOP`), and gives a path-only descriptor for any file.
- F6: privilege: `CAP_DAC_READ_SEARCH` (or the limited 6.10 relaxation), otherwise `EPERM`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Without privilege it fails with an error that depends only on the arguments (`EBADF` in the probe, otherwise `EPERM`), so it repeats. With privilege its input is a host handle whose bytes encode host inode and generation numbers, so a handle only means the same file on the same filesystem instance, and its result depends on whether that inode still exists and on who else is changing the filesystem. The returned descriptor number follows the usual lowest-free-slot rule, and the opened file bypasses path-based access checks, so it reaches files outside any path view Hermit presents.

### Nondeterminism sources

- Host-inode-based handle contents, valid only for one filesystem instance; `ESTALE` once the inode is gone
- `CAP_DAC_READ_SEARCH` and, after Linux 6.10, the filesystem's owning user namespace
- The returned descriptor, which Hermit's descriptor bookkeeping would have to track like any other open
- Concurrent changes to the filesystem (deletion, rename, inode reuse)

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `open_by_handle_at` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724). The comment there lists resolving files by kernel handle among the global kernel state the container pins, and names `CAP_DAC_READ_SEARCH` as the gating capability.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm reads no argument and is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership and runs no guest.

Why `refuse`:

- The companion `name_to_handle_at` returns `EOPNOTSUPP` because "host object handles and mount IDs are outside Detcore's filesystem identity model" (`detcore/src/lib.rs:2593-2595`). That behaviour is covered by guest tests such as `tests/c/name_to_handle.c`. With no way to obtain a valid handle inside the guest, the only handles `open_by_handle_at` could see are forged or carried in from outside, which would expose host inode identities and bypass path-based access.
- `EPERM` is what Linux returns to an unprivileged caller, which the guest is with respect to the host filesystems.
- A `model` policy would need Detcore to mint its own deterministic handles in `name_to_handle_at` and decode them here; that would change both calls together.

Known difference from Linux: Hermit returns `EPERM` before checking `mount_fd` or the handle, so the probe's `mount_fd = -1` case gives `EPERM` instead of `EBADF`, and a bad `handle_bytes` gives `EPERM` instead of `EINVAL`.

Target (owner decision 6): keep the fixed `EPERM`, returned before any argument is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's order is `handle_to_path` in `fs/fhandle.c` (as of Linux 6.10): `mount_fd` is resolved first (`EBADF`; the host probe on Linux 6.17 shows this), then the decode permission check (`may_decode_fh`, `CAP_DAC_READ_SEARCH`, else `EPERM`), then the handle header is copied (`EFAULT`) and `handle_bytes` checked (`EINVAL`). Hermit deliberately does not reproduce the `EBADF` that comes before the permission check. Programs that try handles and fall back to path-based access (for example after `name_to_handle_at` returns `EOPNOTSUPP`) see the permission error an unprivileged Linux caller gets with a valid `mount_fd`.

## Recommended tests

- T1 (covers F1, F3, F4, F5, F6): under `hermit run --verify` on ptrace, a guest builds a 20-byte handle by hand (since `name_to_handle_at` fails) and calls `syscall(SYS_open_by_handle_at, AT_FDCWD, h, flags)` with `O_RDONLY`, `O_RDWR|O_APPEND`, `O_RDONLY|O_CLOEXEC`, and `O_PATH`, and with `mount_fd` a descriptor for `/`. Each must return -1 with `EPERM`, and the next `open` must get the same descriptor number it would have got without the calls.
- T2 (covers F1, F2): pair with `tests/c/name_to_handle.c`: after `name_to_handle_at` fails with `EOPNOTSUPP`, call `open_by_handle_at` with `mount_fd = -1` (the host probe's arguments), `handle_bytes = 0`, `handle_bytes = 129`, and `handle = (void *)-1`. Each returns the fixed `EPERM` of owner decision 6 (Linux gives `EBADF` for the first to any caller, and `EINVAL`, `EINVAL` and `EFAULT` for the others only to a caller that passes the permission check), and both `--verify` runs agree.
- T3 (covers F3, F6): `hermit record` then `hermit replay` the T1 guest; replay must print the same results.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/313#issuecomment-5872890421
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/313: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; open_by_handle_at records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/313 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every open_by_handle_at request, never forwarded, consistent with name_to_handle_at returning EOPNOTSUPP at detcore/src/lib.rs:2595); linux_determinism stays conditional; 6 flag behaviours and 3 tests listed; no coverage token because no test runs a guest that calls open_by_handle_at (tests/c/name_to_handle.c covers only the companion call), so tracking_status stays todo; open question on errno order under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before any argument check stays the target; Linux order in handle_to_path deliberately not reproduced); target_policy stays refuse; T2 now asserts the decision-6 errno; no new F/T ids; open question on checking mount_fd and handle_bytes first removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `open_by_handle_at`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+open_by_handle_at&body=Discussion+for+the+%60open_by_handle_at%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopen_by_handle_at.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60open_by_handle_at%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: open_by_handle_at" (https://github.com/rrnewton/hermit/issues/313), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/313.

The legacy body said a permitted call's returned descriptor "must be virtualized"; under the `refuse` policy no descriptor is returned, and that note applies only if the policy later moves to `model`.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=open_by_handle_at`, and by confirming hermit currently routes `open_by_handle_at` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
open_by_handle_at(-1, {handle_bytes=20, handle_type=77, f_handle="\x99\x25\x03\x00\x00\x00\x00\x00\x05\x00\x00\x00\x00\x00\x00\x00\x0d\x00\x00\x00"}, O_RDONLY) = -1 EBADF (Bad file descriptor)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

The pass-through routing described in that evidence was true when it was filed. At hermit f4055eb9cae4 `open_by_handle_at` is Determinized and refused (see Hermit design).

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/313#issuecomment-5872890421
