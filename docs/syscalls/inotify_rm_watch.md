---
schema: syscall-pages/v1
syscall: inotify_rm_watch
nr_x86_64: 255
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_rm_watch&body=Discussion+for+the+%60inotify_rm_watch%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_rm_watch.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_rm_watch%60%2C+or+in+what+the+page+says%3F%0A
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

`inotify_rm_watch(fd, wd)` removes watch descriptor `wd` from an inotify instance. The kernel then queues an `IN_IGNORED` event for it. Detcore serializes the call at a scheduler point and forwards it unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:4309`, via `notification_fd_control` at `detcore/src/syscalls/files.rs:4254`).

Manual reference: Linux man-pages inotify_rm_watch(2), inotify(7).

## Linux semantics

The kernel looks up `fd` (`EBADF` if it is not open), checks that it is an inotify instance (`EINVAL` otherwise), and then looks up `wd` in that instance's watch table (`EINVAL` if it is not there). On success it detaches the watch and queues `IN_IGNORED` with that `wd` for the reader, after any events already queued for it. The number is not handed out again until the per-instance allocator wraps. A watch that disappeared on its own, through `IN_ONESHOT` or because its inode was deleted, is already gone, so removing it gives `EINVAL`.

### Flags and options

- F1: `wd` names a live watch on this instance: the watch is removed, 0 is returned and `IN_IGNORED` is queued
- F2: `wd` is not a live watch on this instance, including one already removed by `IN_ONESHOT` or by deletion of the watched inode: `EINVAL`
- F3: `fd` is open but not an inotify instance: `EINVAL`
- F4: `fd` is not open: `EBADF`

## Determinism analysis

`linux_determinism: conditional`. The result depends only on the instance's watch table, which the guest built itself, so it repeats for the same sequence of calls. The exception is a watch the kernel removed on its own (F2): whether `IN_ONESHOT` fired, or the watched inode was deleted, before the call depends on filesystem activity, which may come from outside the guest or from other guest threads in a scheduling-dependent order.

### Nondeterminism sources

- Kernel-side removal of the watch, through `IN_ONESHOT` or deletion of the watched inode, racing with the call
- Where `IN_IGNORED` lands relative to events from other writers that are still arriving

## Hermit design

Current behaviour (hermit 239ca43d3092): inotify rename cookies are virtualized on every read path, and blocking reads wait in the scheduler. Linux draws the cookie pairing `IN_MOVED_FROM` with `IN_MOVED_TO` from one host-wide counter, so after a successful `read`, `readv`, or current-offset `preadv2` on an inotify descriptor Detcore replaces each nonzero cookie with a run-wide virtual one, numbered from 1 in first-observation order (`detcore/src/syscalls/inotify.rs:9`); a pair keeps one value however far apart its halves are read, including events copied before a fault and descriptors received from another process. Instances Detcore creates are physically nonblocking with the guest's blocking mode kept by Detcore, so a blocking read that finds nothing queued waits in Detcore's turns instead of in the kernel. The text below predates the renumbering where it says events are not rewritten; event contents other than the cookie fields still come from the kernel.

Target policy: `model`, as for the rest of the inotify family. The dispatch arm is `detcore/src/lib.rs:2587`. `handle_inotify_rm_watch` (`detcore/src/syscalls/files.rs:4309`) calls `notification_fd_control`, which waits for a scheduler turn with `resource_request` (`detcore/src/syscalls/files.rs:4260`) and then runs the call through `record_or_replay`. The kernel keeps the watch table, so all four outcomes are the kernel's. The call is listed as determinized (`detcore/src/syscall_classification.rs:121`) and charged a fixed virtual cost (`detcore/src/syscall_time.rs:118`).

Why `model`: the scheduler point puts the removal at a deterministic place relative to other guest threads' filesystem activity, which is what decides whether `IN_ONESHOT` or a deletion beat it (F2). `passthrough` would lose that. `refuse` breaks every program that stops watching. There is nothing to `emulate`.

What remains: events produced by processes outside the guest still race with the removal, and record and replay are unqualified, as described in the inotify_add_watch body.

## Recommended tests

- T1 (covers F1): add a watch, remove it, then read the instance and check for exactly one `IN_IGNORED` with that `wd`, under `hermit run --strict --verify` on ptrace.
  Existing candidates: `tests/c/inotify_watch.c:41`, which checks that removal returns 0 but reads nothing, run by cell `c-programs/inotify-watch` (`tests/e2e/manifests/c-programs.yaml:12077`, verify ci on ptrace), and `inotify_guest` in `detcore/tests/misc/notification_fds.rs:232`, which asserts 0 after reading its events, run by `inotify_order_is_deterministic` (`detcore/tests/misc/notification_fds.rs:317`).
- T2 (covers F2): remove the same `wd` twice; remove a `wd` after its `IN_ONESHOT` watch fired; remove a `wd` after its watched directory was deleted. Each removal after the first must give `EINVAL`.
- T3 (covers F3, F4): `inotify_rm_watch` on a pipe fd (`EINVAL`) and on a closed fd (`EBADF`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel holds the watch table; Detcore serializes the call through resource_request plus record_or_replay); linux_determinism stays conditional (only kernel-side removal races vary); 4 flag behaviours and 3 tests listed; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 noted (the family blocking-read question this body pointed to is settled; the pointer now names only record/replay); target_policy stays model; no new F/T ids; no question of this call removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for inotify cookie virtualization and scheduler-turn blocking reads; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `inotify_rm_watch`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_rm_watch&body=Discussion+for+the+%60inotify_rm_watch%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_rm_watch.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_rm_watch%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `inotify_rm_watch`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- None specific to this call. The family-wide record/replay qualification question is recorded in the inotify_init and inotify_add_watch bodies. Blocking reads were settled by owner decision 14 (probe plus scheduler wait; see the inotify_init1 body).
