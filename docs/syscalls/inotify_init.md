---
schema: syscall-pages/v1
syscall: inotify_init
nr_x86_64: 253
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_init&body=Discussion+for+the+%60inotify_init%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_init.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_init%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`inotify_init()` creates an inotify instance and returns a descriptor for reading filesystem events. It is the flag-less form of `inotify_init1(0)`. Detcore converts it to that call (`detcore/src/lib.rs:2581`), lets the kernel create the instance through `record_or_replay`, and tracks the new descriptor as `FdType::Inotify` (`detcore/src/syscalls/files.rs:4283`). Because the descriptor is always blocking, a later blocking `read` on it takes Detcore's external-blocking path, which this body flags as a likely determinism hazard.

Manual reference: Linux man-pages inotify_init(2), inotify(7).

## Linux semantics

The call takes no arguments. It allocates an inotify group and returns the lowest free descriptor, with neither `O_NONBLOCK` nor `O_CLOEXEC` set. A `read` on the descriptor waits until an event is queued. `poll`, `select` and `epoll` report it readable when events are pending. `ioctl(FIONREAD)` returns the bytes queued, and `ioctl(INOTIFY_IOC_SETNEXTWD)` chooses the next watch descriptor number (used for checkpoint/restore). The instance is freed when its last descriptor is closed. Each instance's event queue is bounded by `/proc/sys/fs/inotify/max_queued_events`; once full, the kernel drops events and queues one `IN_Q_OVERFLOW`.

Failure modes:

- `EMFILE` when the user already owns `/proc/sys/fs/inotify/max_user_instances` instances. The count is per real user, through each user namespace and its ancestors, so instances held by other host processes of the same user count.
- `EMFILE` when the process has `RLIMIT_NOFILE` descriptors open; `ENFILE` at the system-wide file limit.
- `ENOMEM` when the kernel cannot allocate the group.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

`linux_determinism: conditional`. Creating the instance repeats exactly: the descriptor number is the lowest free one, which depends only on the process's own descriptor table, and the instance starts empty. Two things can change the result:

- The per-user instance limit and the system file limit are shared with host processes outside the guest, so `EMFILE` or `ENFILE` depends on what else the user runs.
- What a guest later reads from the descriptor depends on which processes change the watched paths and when. That belongs to `inotify_add_watch` and `read`, but it is the reason a program creates the instance.

### Nondeterminism sources

- `max_user_instances` usage by other host processes of the same user, and the system-wide file count
- Event timing and content on the descriptor, which depends on writers inside and outside the guest (see the inotify_add_watch body)

## Hermit design

Current behaviour (hermit 239ca43d3092): inotify rename cookies are virtualized on every read path, and blocking reads wait in the scheduler. Linux draws the cookie pairing `IN_MOVED_FROM` with `IN_MOVED_TO` from one host-wide counter, so after a successful `read`, `readv`, or current-offset `preadv2` on an inotify descriptor Detcore replaces each nonzero cookie with a run-wide virtual one, numbered from 1 in first-observation order (`detcore/src/syscalls/inotify.rs:9`); a pair keeps one value however far apart its halves are read, including events copied before a fault and descriptors received from another process. Instances Detcore creates are physically nonblocking with the guest's blocking mode kept by Detcore, so a blocking read that finds nothing queued waits in Detcore's turns instead of in the kernel. The text below predates the renumbering where it says events are not rewritten; event contents other than the cookie fields still come from the kernel.

Target policy: `model`. The kernel holds the instance, its watches and its queue. Detcore owns the descriptor's identity and how operations on it are scheduled:

- The dispatch arm converts `inotify_init` to `inotify_init1` with flags 0 (`detcore/src/lib.rs:2581`; the conversion is Reverie's `From<InotifyInit> for InotifyInit1`). `handle_inotify_init1` executes it through `record_or_replay` (`detcore/src/syscalls/files.rs:4288`) and registers the descriptor with `add_fd` as `FdType::Inotify` (`detcore/src/syscalls/files.rs:4293`). With metadata virtualization on, `add_fd` reads the descriptor's `fstat` and closes the descriptor if that fails, rather than leave an untracked one open (`detcore/src/syscalls/files.rs:924`).
- A `read` on an `FdType::Inotify` descriptor goes to `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`), and `inotify_add_watch` and `inotify_rm_watch` are serialized by the scheduler (see their bodies).
- `/proc/self/fdinfo` for an inotify descriptor is emptied: `sanitize_fdinfo` returns nothing when it sees an `inotify ` line (`detcore/src/procfs.rs:2816`), so host inode numbers and device IDs of the watched files are not exposed.
- The syscall is charged a fixed virtual cost, the same as open/close (`detcore/src/syscall_time.rs:115`), and is listed as determinized (`detcore/src/syscall_classification.rs:119`).

Why `model` rather than the alternatives: `passthrough` would leave the descriptor untracked, so a blocking `read` would sleep inside the kernel where the scheduler cannot see it. `refuse` breaks file watchers such as `tail -f`, build tools and editors. There is no fixed answer to `emulate` with.

Where the code does not yet realize `model`:

- **A blocking read takes the external path.** Detcore records the descriptor's physical nonblocking state from its creation flags (`detcore/src/fd.rs:229`), so a descriptor from `inotify_init` is both logically and physically blocking. `ioaction_based_on_fd_status` then chooses `IOAction::Blocking` (`detcore/src/syscalls/helpers.rs:772`), and `execute_nonblockable_fd_syscall` sends the read to `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:242`). That path deschedules the thread "to run in the background and rejoin nondeterministically" (`detcore/src/syscalls/helpers.rs:156`). If the events the reader waits for are produced by another guest thread, this is the same shape as the documented record/replay pipe hang described in that comment. This is a hypothesis from the code, not a measurement; T2 tests it. Every existing test creates its instance with `IN_NONBLOCK` and never reaches this path.
- **Target (owner decision 14):** a blocking `read` is a non-blocking probe plus a scheduler wait. Since `inotify_init` reaches `handle_inotify_init1` with flags 0, the fix belongs there: add `IN_NONBLOCK` to the call sent to the kernel outside record/replay, keep the guest's flags in the mirror, and mark the description physically nonblocking, as `handle_eventfd2` does (`detcore/src/syscalls/files.rs:4189-4208`). A read the guest believes is blocking then takes `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:778`) instead of `IOAction::Blocking`. Today it takes the external path described above. The call is too widely used to refuse, so `target_policy` stays `model`.
- **Record and replay.** In record/replay mode every descriptor that is not a pipe takes the same blocking path (`detcore/src/syscalls/helpers.rs:242`, `syscall_targets_internal_fd` at `detcore/src/syscalls/helpers.rs:797`), and the replay mode of the existing inotify cell has never run.
- The per-user instance limit is the host's; Hermit does not virtualize it.

## Recommended tests

- T1 (covers none): `syscall(SYS_inotify_init)` under `hermit run --strict --verify` on ptrace returns the lowest free descriptor, with `fcntl(F_GETFL)` showing no `O_NONBLOCK` and `fcntl(F_GETFD)` showing no `FD_CLOEXEC`; `/proc/self/fdinfo/<fd>` reads identically in both runs. Then add a watch, create a file in the watched directory from the same thread, and `read` once; the event is already queued, so the read must not wait.
  Existing tests use only `inotify_init1(IN_NONBLOCK | IN_CLOEXEC)`: `tests/c/inotify_watch.c:35`, `tests/c/event_delivery_ordering.c:63` and `detcore/tests/misc/notification_fds.rs:176`.
- T2 (covers none): a blocking `inotify_init` descriptor where one guest thread blocks in `read` before a second guest thread creates the watched file. Run five times under `--verify` and under `hermit record`/`hermit replay`, with a timeout. A hang or a run-to-run difference confirms the external-blocking hazard in Hermit design.
- T3 (covers none): with `RLIMIT_NOFILE` lowered to the current descriptor count, `inotify_init` returns `EMFILE` under Hermit as natively.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel holds the instance; Detcore tracks FdType::Inotify, routes reads through its nonblockable path and empties fdinfo); linux_determinism stays conditional (only shared per-user limits vary); no flags; 3 tests listed; open question for the owner on the blocking-read path, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target bullet: internal IN_NONBLOCK in handle_inotify_init1, as for eventfd, so blocking reads become scheduler-owned polling; today they take the BlockingExternalIO path); target_policy stays model; no new F/T ids; removed the physically-nonblocking question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for inotify cookie virtualization and scheduler-turn blocking reads; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `inotify_init`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+inotify_init&body=Discussion+for+the+%60inotify_init%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finotify_init.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60inotify_init%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `inotify_init`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Record/replay of inotify descriptors is not qualified: the `replay` mode of cell `c-programs/inotify-watch` is `ci: false` and has never run (`tests/e2e/manifests/c-programs.yaml:12077`).
