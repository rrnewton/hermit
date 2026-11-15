---
schema: syscall-pages/v1
syscall: inotify_init1
nr_x86_64: 294
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_init1&body=Discussion+for+the+%60inotify_init1%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_init1.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_init1%60%2C+or+in+what+the+page+says%3F%0A
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

`inotify_init1(flags)` creates an inotify instance, a kernel event queue for filesystem watches, and returns a descriptor for it. Detcore lets the kernel create the instance and then tracks the descriptor as `FdType::Inotify` with the guest-visible `IN_CLOEXEC`/`IN_NONBLOCK` flags (`detcore/src/syscalls/files.rs:4283`). Reads from the descriptor go through Detcore's blocking-aware path. An instance created without `IN_NONBLOCK` is left blocking in the kernel, though, so a read that waits takes the nondeterministic external-blocking route.

Manual reference: Linux man-pages inotify_init1(2), inotify(7).

## Linux semantics

The call allocates an fsnotify group and returns a new descriptor for it. The descriptor refers to an anonymous inode (`anon_inode:inotify`). Watches are added and removed later with `inotify_add_watch` and `inotify_rm_watch`, and events are read with `read(2)` as a stream of `struct inotify_event` records. The legacy `inotify_init()` (nr 253) is the same call with `flags == 0`. Detcore converts it and sends it to the same handler (`detcore/src/lib.rs:2581`).

The kernel checks the flags first. Any bit outside `IN_CLOEXEC | IN_NONBLOCK` gives `EINVAL` before anything is allocated. Next it charges one instance to the caller's per-user inotify count (`fs.inotify.max_user_instances`, default 128). That count is kept in the user-namespace ucounts and is checked at every level up to the initial namespace, so other processes of the same host user share the budget. The queue limit `fs.inotify.max_queued_events` is copied into the instance when it is created.

The instance lives until the last reference to its open file description is closed. `dup`, `fork` and `SCM_RIGHTS` copies all share one queue, and each event is consumed by whichever reader reads it first.

### Flags and options

- F1: `flags == 0`: the instance is blocking, so `read(2)` waits until an event is queued, and the descriptor survives `execve`. `inotify_init()` has the same behaviour.
- F2: `IN_CLOEXEC` (equal to `O_CLOEXEC`): sets `FD_CLOEXEC` on the new descriptor
- F3: `IN_NONBLOCK` (equal to `O_NONBLOCK`): sets `O_NONBLOCK` on the open file description. A `read(2)` on an empty queue gives `EAGAIN`, and `F_GETFL` reports the flag.
- F4: any other bit in `flags`: `EINVAL`. This is checked before any allocation.
- F5: resource limits:
  - `EMFILE` when the per-user instance count (`fs.inotify.max_user_instances`, charged through the user-namespace hierarchy) is used up, or when `RLIMIT_NOFILE` is reached
  - `ENFILE` at the system-wide file limit
  - `ENOMEM`
- F6: descriptor number: the lowest unused descriptor in the caller's table
- F7: instance identity:
  - `readlink` of `/proc/self/fd/N` gives `anon_inode:inotify`
  - `fstat` reports the shared anonymous inode
  - `/proc/self/fdinfo/N` lists one `inotify wd:... ino:... sdev:... mask:...` line per watch
- F8: queue limit: `fs.inotify.max_queued_events` is read when the instance is created. Once the queue is full, further events are dropped and a single `IN_Q_OVERFLOW` event is queued.
- F9: sharing and lifetime: `dup`, `fork` and `SCM_RIGHTS` copies share one queue, and an event is delivered to exactly one reader. Closing the last reference destroys the instance and all its watches.
- F10: content of events read from the instance:
  - An `IN_MOVED_FROM`/`IN_MOVED_TO` pair carries a cookie taken from a counter that is global to the host.
  - An event identical to the last unread queued event is merged with it.
  - A read buffer smaller than the next event gives `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`. Creating the instance is deterministic given the same `flags`, descriptor table and resource limits. The returned descriptor number depends only on the caller's own table (F6). What the instance later delivers depends on which filesystem changes happen under its watches, and in what order. Those changes include ones made by processes outside the guest. The call and its event stream repeat only when every writer to the watched paths is inside one deterministic schedule and the host limits are not close to exhaustion.

### Nondeterminism sources

- The per-user instance budget (F5) is shared with every other process of the same host user, so `EMFILE` can appear or disappear between runs.
- Host sysctls `fs.inotify.max_user_instances` and `fs.inotify.max_queued_events` (F5, F8)
- Host processes that modify paths the guest watches. They add events, change merging (F10), and can cause `IN_Q_OVERFLOW` (F8).
- The rename cookie (F10) comes from a counter shared by the whole host. Two runs of the same guest are expected to see different cookie values. This is a reading of the kernel's `fsnotify_get_cookie`, not a measurement; T7 measures it.
- When a blocking read (F1) returns, relative to other threads, is decided by host scheduling.

## Hermit design

Current behaviour (hermit 239ca43d3092): inotify rename cookies are virtualized on every read path, and blocking reads wait in the scheduler. Linux draws the cookie pairing `IN_MOVED_FROM` with `IN_MOVED_TO` from one host-wide counter, so after a successful `read`, `readv`, or current-offset `preadv2` on an inotify descriptor Detcore replaces each nonzero cookie with a run-wide virtual one, numbered from 1 in first-observation order (`detcore/src/syscalls/inotify.rs:9`); a pair keeps one value however far apart its halves are read, including events copied before a fault and descriptors received from another process. Instances Detcore creates are physically nonblocking with the guest's blocking mode kept by Detcore, so a blocking read that finds nothing queued waits in Detcore's turns instead of in the kernel. The text below predates the renumbering where it says events are not rewritten; event contents other than the cookie fields still come from the kernel.

Target policy: `model`. The kernel stays the owner of the event queue and the watch table. Detcore owns the descriptor, the guest-visible flags, and every decision about waiting. What the code does at f4055eb9cae4:

- Creation is sent to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:4288`), so flag validation (F4), limits (F5) and descriptor allocation (F6) are Linux's own. In record/replay the result is recorded.
- The descriptor is registered with `add_fd` as `FdType::Inotify` (`detcore/src/syscalls/files.rs:4289`; enum variant at `detcore/src/fd.rs:55`). Only `IN_CLOEXEC | IN_NONBLOCK` are kept as its flags (`detcore/src/syscalls/files.rs:4292`). When `virtualize_metadata` is on, `add_fd` injects an `fstat` and records the result (`detcore/src/syscalls/files.rs:923`). If that `fstat` fails, it closes the new descriptor rather than leave an untracked one open (`detcore/src/syscalls/files.rs:935`).
- `read` on an `FdType::Inotify` descriptor goes to `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`). An instance created with `IN_NONBLOCK` is nonblocking both logically and physically, (the default set at `detcore/src/fd.rs:229`), so the read is issued once (`IOAction::PassThru`, `detcore/src/syscalls/helpers.rs:773`). Events caused by the guest's own filesystem calls are queued synchronously inside those calls, so under the sequentialized schedule such a read sees a repeatable queue.
- `/proc/self/fdinfo` for an instance with watches is replaced by empty contents: any line starting with `inotify ` makes `sanitize_fdinfo` return nothing (`detcore/src/procfs.rs:2815`, under the code tag `TODO-HUMAN-REVIEW(PR-931)` at `detcore/src/procfs.rs:2790`). This is deterministic, but differs from Linux for F7.
- The call is charged the `OPEN_CLOSE_NS` virtual-time cost (`detcore/src/syscall_time.rs:116`). It stays intercepted in the `passthru_opt` allow-list (`detcore/src/lib.rs:1181`).

Why `model` and not the alternatives: `passthrough` would leave the descriptor untracked, so reads on it could not be scheduled. `emulate` would mean re-implementing fsnotify's event generation in Detcore, which the kernel already does deterministically for the guest's own changes. `refuse` would break file watchers such as build tools, `tail -F` and editors.

What remains before the code fully realizes `model`:

- **Blocking instances (F1).** `handle_inotify_init1` does not make the instance physically nonblocking. Compare `handle_eventfd2`, which adds `EFD_NONBLOCK` internally when `use_nonblocking_sockets()` is set outside record/replay (`detcore/src/syscalls/files.rs:4189`). `F_SETFL` also forces physical nonblocking only for `Socket`, `Pipe` and `Eventfd` (`detcore/src/syscalls/files.rs:3113`). A blocking read on an inotify descriptor is therefore logically and physically blocking, which maps to `IOAction::Blocking` (`detcore/src/syscalls/helpers.rs:770`) and then to `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:241`). That path uses `ResourceID::BlockingExternalIO` (`detcore/src/syscalls/helpers.rs:118`). The comment at `detcore/src/syscalls/helpers.rs:156` says this deschedules the thread and lets it "rejoin nondeterministically". The fix that matches the eventfd model is an internal `IN_NONBLOCK` on creation, so reads take the `NonblockizeRetry` polling path (`detcore/src/syscalls/helpers.rs:778`). T4 should expose the current behaviour.
- **Target (owner decision 14):** a blocking `read` on an inotify instance is a non-blocking probe plus a scheduler wait. Concretely, `handle_inotify_init1` adds `IN_NONBLOCK` to the call it sends to the kernel whenever `use_nonblocking_sockets()` holds outside record/replay, registers the guest's own flags, and marks the description physically nonblocking, exactly as `handle_eventfd2` does (`detcore/src/syscalls/files.rs:4189-4208`); `F_SETFL` keeps forcing `O_NONBLOCK` for `FdType::Inotify` as it does for `Eventfd` (`detcore/src/syscalls/files.rs:3113`). A read the guest believes is blocking then takes `NonblockizeRetry` under `InternalIOPolling` (`detcore/src/syscalls/helpers.rs:778`). Today the instance is created exactly as asked (`detcore/src/syscalls/files.rs:4288-4295`), so a blocking read waits in the kernel through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`). File watchers are too widely used to refuse, so `target_policy` stays `model`.
- **Record/replay.** Only `FdType::Pipe` counts as a container-internal descriptor (`detcore/src/syscalls/helpers.rs:804`), so in record/replay every inotify read uses the external-blocking path (`detcore/src/syscalls/helpers.rs:242`).
- **Rename cookies (F10).** No code under `detcore/src` rewrites inotify event records, so a cookie taken from the host-wide counter reaches the guest unchanged. If T7 confirms that the value differs between runs, Detcore has to remap cookies to a per-container sequence.
- **Instance budget (F5).** `EMFILE` from the per-user instance count is passed through. Hermit does not virtualize that limit.

`linux_determinism` stays `conditional`. Hermit removes the scheduling source for nonblocking instances. It does not remove the host-writer source, the shared limits, or the cookie counter.

## Recommended tests

- T1 (covers F1, F2, F3, F6): create instances with `0`, `IN_CLOEXEC`, `IN_NONBLOCK`, and both flags. Check that each descriptor is the lowest free number, and that `F_GETFD` and `F_GETFL` match the flags. A read on an empty `IN_NONBLOCK` queue must give `EAGAIN`.
  Existing partial coverage: `tests/c/inotify_watch.c` (cell `c-programs/inotify-watch`, `tests/e2e/manifests/c-programs.yaml:12077`, verify ci). It creates only `IN_NONBLOCK|IN_CLOEXEC` instances, adds and removes a watch on `/tmp`, and never reads.
- T2 (covers F4): `syscall(SYS_inotify_init1, IN_NONBLOCK | 0x1)` and `syscall(SYS_inotify_init1, 0x40000000)` give `EINVAL`. The next `open` must return the same descriptor number as before the failed calls, showing that no descriptor was used.
- T3 (covers F5): lower `RLIMIT_NOFILE` to the number of open descriptors and check that `inotify_init1` gives `EMFILE`. Then raise the limit, and check that the call succeeds and no descriptor leaked. Do not depend on `fs.inotify.max_user_instances`, which the host shares.
- T4 (covers F1, F9): thread A blocks in `read` on an instance created with `flags == 0`. Thread B then creates two files in the watched directory. A forked child inherits the descriptor, reads once, and must see events that the parent did not. Run under `hermit run --verify` on ptrace and compare the event order printed. This test exposes the `BlockingExternalIO` gap described under Hermit design.
- T5 (covers F7): check that `readlink("/proc/self/fd/N")` is `anon_inode:inotify`, that `fstat` reports the same `st_mode` and `st_ino` in every run, and what `/proc/self/fdinfo/N` contains with no watch and with one watch. Hermit's empty fdinfo when a watch exists is recorded as the expected Hermit result.
- T6 (covers F8): read `/proc/sys/fs/inotify/max_queued_events`, and generate that many distinct events plus one without reading. Alternating `open`/`close` of one watched file works, because consecutive events then differ and are not merged. Then check that the last event read is a single `IN_Q_OVERFLOW`.
- T7 (covers F10): rename a file inside a watched directory and print the raw cookie from `IN_MOVED_FROM`/`IN_MOVED_TO`. Under `--verify`, compare the cookie across runs. Also write the same file twice before reading, to check merging, and read with a buffer of `sizeof(struct inotify_event)` when the next event carries a name, which must give `EINVAL`.
  Existing candidates for the merge and ordering part: `inotify_order_is_deterministic` at `detcore/tests/misc/notification_fds.rs:317`, which runs an `IN_CLOEXEC|IN_NONBLOCK` guest five times under Detcore (`detcore/tests/misc/notification_fds.rs:176`). Also `tests/c/event_delivery_ordering.c` (instance created at line 63; cell `c-programs/event-delivery-ordering`, `tests/e2e/manifests/c-programs.yaml:17039`, verify ci on ptrace with `bitwise_parity`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel owns the queue; Detcore tracks the fd as FdType::Inotify and schedules its reads); linux_determinism stays conditional; 10 flag behaviours and 7 tests listed; gaps recorded: blocking instances stay physically blocking and wait through BlockingExternalIO, and rename cookies are not remapped; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target bullet: an internal IN_NONBLOCK at creation and F_SETFL forcing, as for eventfd, so blocking reads become scheduler-owned polling; today a blocking read waits in the kernel); target_policy stays model; no new F/T ids; removed the internal-IN_NONBLOCK question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for inotify cookie virtualization and scheduler-turn blocking reads; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `inotify_init1`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_init1&body=Discussion+for+the+%60inotify_init1%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_init1.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_init1%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `inotify_init1`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Is the host-global rename cookie (F10) a real determinism leak under `--verify`? T7 measures it. If it is, the remapping belongs in the read path for `FdType::Inotify`.
- Is an empty `/proc/self/fdinfo` for an instance with watches (`detcore/src/procfs.rs:2815`) the intended long-term policy, or should Detcore print virtualized `inotify wd:` lines?
