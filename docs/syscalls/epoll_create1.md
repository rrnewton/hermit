---
schema: syscall-pages/v1
syscall: epoll_create1
nr_x86_64: 291
linux_determinism: deterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_create1&body=Discussion+for+the+%60epoll_create1%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_create1.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_create1%60%2C+or+in+what+the+page+says%3F%0A
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

`epoll_create1(flags)` creates an epoll instance and returns a descriptor for it. The instance holds an interest list and a ready list. Detcore handles the call at `detcore/src/syscalls/io.rs:1143`. It makes an empty scheduler request, lets the kernel create the instance through `record_or_replay`, and registers the descriptor in its mirror. That registration is what lets `dup`, `F_DUPFD_CLOEXEC` and `F_SETFD` on an epoll descriptor work.

Manual reference: Linux man-pages epoll_create(2).

## Linux semantics

The call returns the lowest free descriptor. The only valid flag is `EPOLL_CLOEXEC`, which has the same value as `O_CLOEXEC`; any other bit gives `EINVAL`.

The interest list belongs to the open file description. Copies made by `dup` or `fork`, or sent through `SCM_RIGHTS`, share it. The instance is freed when its last descriptor is closed.

Creating an instance has no effect beyond the descriptor table and memory accounting. Readiness and event order come from the later `epoll_ctl` and `epoll_wait` calls.

The older `epoll_create(size)` (nr 213) requires `size > 0`, otherwise it fails with `EINVAL`. Apart from that check it ignores `size`, and it creates the instance as `epoll_create1(0)` does.

### Flags and options

- F1: `flags = 0`: a new instance on a descriptor that is not close-on-exec
- F2: `EPOLL_CLOEXEC`: the new descriptor is close-on-exec
- F3: any other bit in `flags`: `EINVAL`
- F4: resource errors: `EMFILE` (per-process limit), `ENFILE` (system-wide limit), `ENOMEM`
- F5: `epoll_create(size)` (nr 213): `EINVAL` if `size <= 0`; otherwise `size` is ignored and the call acts like `epoll_create1(0)`
- F6: description sharing: descriptors made by `dup`, `F_DUPFD`, `F_DUPFD_CLOEXEC` or `fork` refer to the same instance and its interest list

## Determinism analysis

`linux_determinism: deterministic`. The descriptor number and its close-on-exec state depend only on `flags` and on the calling process's descriptor table, which Hermit serializes. The resource errors in F4 depend on host limits, as they do for every call that creates a descriptor. Readiness order, which is the real source of nondeterminism in epoll, belongs to `epoll_wait` and is tracked there.

## Hermit design

Target policy: `model`. The kernel holds the epoll instance, and Detcore mirrors the descriptor.

- The handler first makes an empty `resource_request` (`detcore/src/syscalls/io.rs:1149`), which gives the scheduler a switch point, then sends the call to the kernel through `record_or_replay` (`detcore/src/syscalls/io.rs:1150`). The kernel therefore decides F3 and F4.
- The descriptor is registered as `FdType::Epoll`, and the guest's `flags` are passed straight through as `O_CLOEXEC` (`detcore/src/syscalls/io.rs:1161` to `detcore/src/syscalls/io.rs:1165`; the bit equivalence is noted at `detcore/src/syscalls/io.rs:1159`) (F1, F2). `DetFd::new` keeps the bit as the close-on-exec flag (`detcore/src/fd.rs:204`).
- The comment at `detcore/src/syscalls/io.rs:1151` records why the registration exists. Without it, `dup` and `fcntl` on the descriptor returned `EBADF`, which broke rustup's proxies, because their tokio runtime duplicates its epoll descriptor at startup (F6).
- `epoll_create` is routed to the same handler through `EpollCreate1::from` (`detcore/src/lib.rs:2618` to `detcore/src/lib.rs:2621`) (F5).

Detcore neither refuses nor emulates anything here, and `passthrough` would bring back the `EBADF` failure above. So `model` is what the code already does.

Open items:

- **`epoll_create(0)` and `epoll_create(-1)`.** The conversion in the reverie checkout of this parent repository (`impl From<EpollCreate> for EpollCreate1` in `reverie-syscalls/src/syscalls.rs`, not pinned to this hermit SHA) overwrites the first argument with flags 0, and its own comment says this "could have changed behavior when calling `epoll_create(-1)`". By that reading, Hermit gives a working descriptor where Linux returns `EINVAL`. This is not measured; T3 checks it. If confirmed, it belongs to `epoll_create`'s tracking body, which this body does not cover.
- **Record and replay.** The Recorder's subscription list (`hermit-cli/src/recorder.rs:384` to `hermit-cli/src/recorder.rs:474`) contains `epoll_wait` but not `epoll_create1`, so creation runs live again on replay. Creation is deterministic, so this is expected to be harmless. The parent-repository report `ai_docs/hermit-record-mode-current-state-20260915.md` (measured at an earlier hermit SHA, not re-run here) lists an `epoll_pipe` program using `epoll_create1` as passing record and replay.

## Recommended tests

- T1 (covers F1, F2, F6): create instances with `flags = 0` and with `EPOLL_CLOEXEC`. Check `F_GETFD`; that `F_GETFL`, `F_SETFD`, `dup`, `F_DUPFD` and `F_DUPFD_CLOEXEC` on the descriptor succeed; and that a `dup` copy sees an `epoll_ctl` registration made through the original.
  Existing candidate: `epoll_fd_supports_descriptor_table_ops` (`hermit-cli/tests/epoll_determinism.rs:416`). It runs `tests/c/epoll_determinism.c` with the `dupfd` scenario (`tests/c/epoll_determinism.c:427`) through `hermit run`, with no `--backend` argument (`hermit-cli/tests/epoll_determinism.rs:88`). The scenario creates the instance with `EPOLL_CLOEXEC` (`tests/c/epoll_determinism.c:39`), does not try `flags = 0`, and does not check that a registration is shared.
  Also: `mixed_epoll_sources_are_deterministic` (`detcore/tests/misc/notification_fds.rs:322`, cargo test target `tests_misc`), which creates its instance with `EPOLL_CLOEXEC` (`detcore/tests/misc/notification_fds.rs:257`), and the cell `c-programs/epoll-readiness` (`tests/e2e/manifests/c-programs.yaml:11508`, verify ci on ptrace).
- T2 (covers F3, F4): `epoll_create1(1)` returns `EINVAL`. With `RLIMIT_NOFILE` lowered to the number of open descriptors, the call returns `EMFILE` and opens nothing.
- T3 (covers F5): raw `syscall(SYS_epoll_create, 1)` succeeds, and `syscall(SYS_epoll_create, 0)` and `syscall(SYS_epoll_create, -1)` return `EINVAL`, under `hermit run --verify` on ptrace and natively.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel instance; Detcore registers the descriptor so dup and fcntl work); linux_determinism deterministic; 6 flag behaviours and 3 tests listed; no coverage token, tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_create1`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_create1&body=Discussion+for+the+%60epoll_create1%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_create1.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_create1%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_create1`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- None for `epoll_create1`. The `epoll_create(size <= 0)` behaviour (T3) belongs to `epoll_create`'s tracking body; it is noted here because the two calls share one handler.
