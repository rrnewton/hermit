---
schema: syscall-pages/v1
syscall: inotify_add_watch
nr_x86_64: 254
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_add_watch&body=Discussion+for+the+%60inotify_add_watch%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_add_watch.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_add_watch%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`inotify_add_watch(fd, pathname, mask)` adds a watch for the file or directory at `pathname` to an inotify instance, or updates the existing watch on that inode, and returns its watch descriptor. Detcore serializes the call at a scheduler point and forwards it unchanged to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:4300`, which calls `notification_fd_control` at `detcore/src/syscalls/files.rs:4254`). The call itself repeats under Hermit; the events the watch later produces depend on who changes the watched path, and Detcore does not rewrite them.

Manual reference: Linux man-pages inotify_add_watch(2), inotify(7).

## Linux semantics

The kernel checks arguments in this order: `IN_MASK_ADD` together with `IN_MASK_CREATE` gives `EINVAL`; a mask with no valid inotify bit gives `EINVAL`; then `fd` is looked up (`EBADF`) and must be an inotify instance (`EINVAL`); then `pathname` is resolved, following a final symlink unless `IN_DONT_FOLLOW` is set, and the caller needs read permission on the target.

A watch belongs to the inode, not the name. Adding a second watch for the same inode on the same instance returns the existing watch descriptor and replaces its mask, or ORs into it with `IN_MASK_ADD`. Watch descriptors are allocated per instance, cyclically, starting at 1. A removed number is not reused until the allocator wraps, so the numbers depend only on the order of the instance's own add and remove calls.

Each event read from the instance is a `struct inotify_event` holding the watch descriptor, the event mask, a `cookie` and an optional name. The cookie pairs an `IN_MOVED_FROM` with its `IN_MOVED_TO` and comes from one counter shared by the whole host. Identical consecutive events on the same watch are merged when the older one has not been read yet. A full queue drops further events and appends one `IN_Q_OVERFLOW`.

### Flags and options

- F1: file content and access events: `IN_ACCESS`, `IN_MODIFY`, `IN_ATTRIB`, `IN_OPEN`, `IN_CLOSE_WRITE`, `IN_CLOSE_NOWRITE` (`IN_CLOSE` is both close bits)
- F2: directory entry events: `IN_CREATE`, `IN_DELETE`, `IN_MOVED_FROM`, `IN_MOVED_TO` (`IN_MOVE` is both move bits). A rename reports both halves with the same `cookie`, drawn from a host-wide counter. Measured: three runs gave cookies 1495250, 1495253 and 1495259.
- F3: events on the watched object itself: `IN_DELETE_SELF`, `IN_MOVE_SELF`; the kernel then also queues `IN_IGNORED` (and `IN_UNMOUNT` when the filesystem is unmounted)
- F4: `IN_ONLYDIR`: watch only if `pathname` is a directory, otherwise `ENOTDIR`
- F5: `IN_DONT_FOLLOW`: do not follow a final symbolic link; the watch is placed on the link itself
- F6: `IN_EXCL_UNLINK`: stop reporting events for children after they are unlinked from the watched directory
- F7: `IN_MASK_ADD`: OR the new events into an existing watch's mask instead of replacing it
- F8: `IN_MASK_CREATE`: fail with `EEXIST` if the inode is already watched by this instance
- F9: `IN_MASK_ADD` together with `IN_MASK_CREATE`: `EINVAL`
- F10: `IN_ONESHOT`: remove the watch after its first event and queue `IN_IGNORED`
- F11: same inode added twice: the existing watch descriptor is returned and its mask updated. Measured: the same directory added twice returned 1 both times.
- F12: watch descriptor numbering: per instance, cyclic, starting at 1
- F13: a mask with no valid inotify bit: `EINVAL`, checked before `fd`
- F14: `fd` not open: `EBADF`; `fd` open but not an inotify instance: `EINVAL`
- F15: path errors: `ENOENT`, `EACCES` (no read permission), `EFAULT`, `ENAMETOOLONG`, `ELOOP`, `ENOTDIR` for a non-directory path component
- F16: `ENOSPC` when the user reaches `/proc/sys/fs/inotify/max_user_watches`, counted across every process of that user; `ENOMEM` on allocation failure

## Determinism analysis

`linux_determinism: conditional`. The return value repeats when the path, permissions and the instance's own history repeat, because watch descriptor numbers come only from that history (F11, F12). The events the watch produces are where it varies:

- Any process that can change the watched path generates events, including processes outside the guest when the path is shared with the host.
- The rename cookie (F2) is a host-wide counter, so its value differs between runs even for an identical guest.
- Merging of identical events and `IN_Q_OVERFLOW` depend on how far the reader lagged behind the writer, which is scheduling order, and on the host's `max_queued_events` setting.
- `ENOSPC` (F16) depends on watches held by other processes of the same user.

### Nondeterminism sources

- Writers outside the guest that change a watched path
- The host-wide rename cookie counter, visible through `read`
- Reader and writer interleaving, which decides event merging and overflow
- Per-user watch limit and the host `max_queued_events` sysctl

## Hermit design

Current behaviour (hermit 239ca43d3092): inotify rename cookies are virtualized on every read path, and blocking reads wait in the scheduler. Linux draws the cookie pairing `IN_MOVED_FROM` with `IN_MOVED_TO` from one host-wide counter, so after a successful `read`, `readv`, or current-offset `preadv2` on an inotify descriptor Detcore replaces each nonzero cookie with a run-wide virtual one, numbered from 1 in first-observation order (`detcore/src/syscalls/inotify.rs:9`); a pair keeps one value however far apart its halves are read, including events copied before a fault and descriptors received from another process. Instances Detcore creates are physically nonblocking with the guest's blocking mode kept by Detcore, so a blocking read that finds nothing queued waits in Detcore's turns instead of in the kernel. The text below predates the renumbering where it says events are not rewritten; event contents other than the cookie fields still come from the kernel.

Target policy: `model`. The kernel holds the watch table and generates events. Detcore owns when control operations happen and what the guest learns about the descriptor:

- The dispatch arm is `detcore/src/lib.rs:2586`. `handle_inotify_add_watch` (`detcore/src/syscalls/files.rs:4300`) calls `notification_fd_control`, which first waits for a scheduler turn with `resource_request` (`detcore/src/syscalls/files.rs:4260`) and then executes the call through `record_or_replay`. The path and mask go to the kernel unchanged, so every argument check and error in F4-F16 is the kernel's, in the kernel's order.
- Reading events goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`). On an `IN_NONBLOCK` descriptor the read is a single nonblocking call; on a blocking descriptor it takes the external-blocking path described in the inotify_init body. The event buffer is returned as the kernel wrote it.
- `/proc/self/fdinfo` for the instance is emptied (`detcore/src/procfs.rs:2816`), so the watch list with host inode and device numbers is hidden.
- The call has a fixed virtual cost (`detcore/src/syscall_time.rs:117`) and is listed as determinized (`detcore/src/syscall_classification.rs:118`).

Why `model`: `passthrough` without the scheduler point would let control operations run at host-timed moments relative to other guest threads. `refuse` breaks every file watcher. The watched filesystem is the kernel's, so there is nothing to `emulate`.

Where the code does not yet realize `model`:

- The rename cookie (F2) reaches the guest unchanged. Detcore would have to rewrite `cookie` in each `struct inotify_event` it returns, mapping host values to a per-run sequence.
- Events caused by processes outside the guest reach it, as for any shared filesystem. Hermit's answer to that is the container's filesystem view, not this handler.
- Record and replay of inotify is unqualified: the `replay` mode of cell `c-programs/inotify-watch` is `ci: false` and has never run (`tests/e2e/manifests/c-programs.yaml:12077`).
- Target (owner decision 14): this call never waits, but the reads it makes useful do. A blocking read on the instance becomes a non-blocking probe plus a scheduler wait through an internal `IN_NONBLOCK` at creation, recorded in the inotify_init1 body; today it takes the external-blocking path.

## Recommended tests

- T1 (covers F2, F11, F12, F14): add, re-add and remove a watch under `hermit run --strict --verify` on ptrace; check the watch descriptor numbers (1, again 1 for the same inode, then 2 for a new inode after removing the first), and `EBADF`/`EINVAL` for a closed fd and for a pipe fd.
  Existing candidate: `tests/c/inotify_watch.c:38`, which adds a watch with `IN_CREATE | IN_DELETE` on `/tmp` and removes it but withholds the number, run by cell `c-programs/inotify-watch` (`tests/e2e/manifests/c-programs.yaml:12077`, verify ci on ptrace).
- T2 (covers F1, F2): create, write and close two files in a watched private directory, then read and print every event mask and name; the output must match across five runs and across `--verify`.
  Existing candidates: `inotify_order_is_deterministic` at `detcore/tests/misc/notification_fds.rs:317`, which reads six `IN_CREATE`/`IN_MODIFY`/`IN_CLOSE_WRITE` events five times, and `tests/c/event_delivery_ordering.c:69`, which watches `IN_MODIFY` and counts merged events, run by cell `c-programs/event-delivery-ordering` (`tests/e2e/manifests/c-programs.yaml:17039`, verify ci on ptrace with `bitwise_parity`).
- T3 (covers F2): rename a file inside a watched directory and print the cookie of the `IN_MOVED_FROM`/`IN_MOVED_TO` pair. Under `--verify` the two runs differ today; this measures the cookie leak.
- T4 (covers F3, F10): delete the watched directory, and separately trigger an `IN_ONESHOT` watch; check for `IN_DELETE_SELF` then `IN_IGNORED`, and for removal after the first event.
- T5 (covers F4, F5, F6, F7, F8, F9, F13): `IN_ONLYDIR` on a regular file (`ENOTDIR`), `IN_DONT_FOLLOW` on a symlink, `IN_EXCL_UNLINK` with an unlinked open child, `IN_MASK_ADD` merging, `IN_MASK_CREATE` on an existing watch (`EEXIST`), both mask flags together (`EINVAL`), and mask 0 on fd -1 (`EINVAL`, not `EBADF`).
- T6 (covers F15, F16): `ENOENT`, `EACCES`, `ELOOP`, `ENAMETOOLONG` and a bad pointer (`EFAULT`). `ENOSPC` needs a lowered `max_user_watches`, so record it only where the test can set that sysctl.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel holds the watch table; Detcore serializes the call through resource_request plus record_or_replay and hides fdinfo); linux_determinism stays conditional (outside writers, the host-wide rename cookie, merging and per-user limits); 16 flag behaviours and 6 tests listed; open question for the owner on virtualizing the rename cookie, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target bullet pointing blocking reads at the probe-plus-wait form in inotify_init1); target_policy stays model; no new F/T ids; removed the physically-nonblocking question (the cookie question stays)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for inotify cookie virtualization and scheduler-turn blocking reads; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `inotify_add_watch`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_add_watch&body=Discussion+for+the+%60inotify_add_watch%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_add_watch.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_add_watch%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `inotify_add_watch`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should Detcore rewrite the `cookie` field of `IN_MOVED_FROM`/`IN_MOVED_TO` events to a per-run sequence (F2, T3)? Today a guest that prints or keys on cookies differs between runs.

### 2026-10-03 native probe (host kernel 7.1.3, unprivileged user)

Python ctypes against libc. Adding a watch on the same directory twice returned watch descriptor 1 both times. Renaming a file inside a watched directory produced an `IN_MOVED_FROM`/`IN_MOVED_TO` pair whose cookie was 1495250, 1495253 and 1495259 in three consecutive runs. Host limits: `max_user_instances` 8192, `max_user_watches` 10000000, `max_queued_events` 1000000.
