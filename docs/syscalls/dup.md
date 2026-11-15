---
schema: syscall-pages/v1
syscall: dup
nr_x86_64: 32
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+dup&body=Discussion+for+the+%60dup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60dup%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`dup` returns a new file descriptor that refers to the same open file description as `oldfd`, using the lowest free descriptor number. Under Hermit the kernel performs the call and picks the number, and Detcore copies its own record of the descriptor so that later calls on the copy see the same file state.

## Linux semantics

`dup(oldfd)` allocates the lowest-numbered descriptor not currently open in the process's descriptor table and makes it refer to the open file description of `oldfd`. The two descriptors then share the file offset, the file status flags (`O_APPEND`, `O_NONBLOCK` and the others `F_SETFL` changes) and `flock` locks, which belong to the open file description. The descriptor flags are not shared: the copy always starts with `FD_CLOEXEC` clear. `dup2` and `dup3` choose the target number and are tracked separately.

Threads created with `CLONE_FILES` share one descriptor table, so a `dup` in one thread races with `open`, `close` and other descriptor allocation in the others.

### Flags and options

- F1: the new descriptor is the lowest-numbered descriptor not open in the calling process's table.
- F2: the new descriptor has `FD_CLOEXEC` clear, whatever the flag on `oldfd`.
- F3: both descriptors share one open file description: file offset, file status flags and `flock` locks. Closing one leaves the description open through the other.
- F4: an `oldfd` that is not open gives `EBADF`.
- F5: when no descriptor below `RLIMIT_NOFILE` is free, the call gives `EMFILE`.
- F6: any kind of open descriptor can be duplicated, including `O_PATH` descriptors, pipes, sockets, epoll, eventfd, timerfd, signalfd and memfd descriptors.

## Determinism analysis

The result is a function of the descriptor table and `RLIMIT_NOFILE`. The table is deterministic when the descriptors inherited at start are the same and the process's own open/close history is the same. Two things break that on Linux:

- Threads that share a descriptor table and allocate descriptors concurrently: which thread gets the lower number depends on host scheduling.
- Processes that share an open file description (after `fork`, or through `SCM_RIGHTS`) and read or write it concurrently: the shared offset (F3) makes the bytes each one sees depend on the interleaving.

In a single-threaded process with the same inherited descriptors, `dup` repeats exactly. That makes `linux_determinism` `conditional`.

## Hermit design

Target policy: `model`. The kernel keeps the descriptor table and allocates the number. Detcore keeps its own per-descriptor record (a `DetFd`), which it needs for later decisions about the file, and updates it after every `dup`. The dispatch arm is `detcore/src/lib.rs:2533`, and the handler is `handle_dup` at `detcore/src/syscalls/files.rs:3517`:

- The call goes to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:3523`). The kernel picks the lowest free number (F1) and returns `EBADF` (F4) and `EMFILE` (F5), and an error returns before Detcore changes anything.
- After a successful call, Detcore copies the record through `dup_fd` (`detcore/src/syscalls/files.rs:3524-3526`). The public `dup_fd` (`detcore/src/tool_local.rs:2627-2638`) first discovers `oldfd` from the live process when `discover_live_file_metadata` is set (`detcore/src/tool_local.rs:2634-2636`). The inner `dup_fd` (`detcore/src/tool_local.rs:787-804`) clones the `DetFd` of `oldfd` with the new number and empty descriptor flags (F2). The clone keeps the same open-file identity, so the two descriptors share Detcore's view of the open file description (F3).
- If the new number replaced an entry Detcore still held, and that was the last alias of its open file, Detcore releases the port it had reserved for that file (`detcore/src/syscalls/files.rs:3527-3528`).
- Thread interleaving is fixed by the sequentialized scheduler, so concurrent descriptor allocation (the Linux source above) happens in the same order in every run.
- In record mode the recorder handles `dup` with `handle_simple` (`hermit-cli/src/recorder.rs:598`); the replayer re-performs it as a replayed side effect (`hermit-cli/src/replayer.rs:478-481`).

Why `model` and not the alternatives:

- `passthrough` would leave Detcore without a record for the new descriptor, and every later operation that looks up the descriptor table (`fcntl`, `flock`, another `dup`) would fail with `EBADF`; the comment at `detcore/src/syscalls/io.rs:1151-1157` records that failure for epoll descriptors.
- `emulate` would mean Detcore allocating descriptor numbers itself, which it does not do for any descriptor-creating call.
- `refuse` would break shells and every program that redirects standard streams.

What remains before the code fully realizes `model`:

- **A descriptor Detcore does not know.** If `oldfd` is open in the kernel but missing from Detcore's table, and live discovery is off, the kernel `dup` has already succeeded when the inner `dup_fd` returns `EBADF` (`detcore/src/tool_local.rs:740`). The guest then sees `EBADF`, and the new kernel descriptor stays open with nothing referring to it. The comment at `detcore/src/syscalls/io.rs:1151-1157` describes this failure having happened for epoll descriptors before they were registered. This is a hypothesis from reading the code for the remaining descriptor kinds; T5 confirms or refutes it.
- **No test exercises `EMFILE` (F5).**

`linux_determinism` stays `conditional`. Under Hermit the number is deterministic because the scheduler fixes the order of descriptor allocation.

## Recommended tests

- T1 (covers F1, F2, F3): `dup` shares the file offset: writes through both descriptors advance one offset. The copy has `FD_CLOEXEC` clear even when `oldfd` has it set. The run must be identical under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/fd_duplication.c` (cell `c-programs/fd-duplication`, `tests/e2e/manifests/c-programs.yaml:15855`, verify ci on ptrace) and `tests/c/dup_shared_offset.c` (cell `c-programs/dup-shared-offset`, `tests/e2e/manifests/c-programs.yaml:15923`, verify ci on ptrace).
- T2 (covers F1): after closing descriptor 3, `dup` returns 3.
  Existing candidate: `detcore/tests/lit/dup_should_create_a_valid_fd/main.rs`. Its `hermit-run.lit` passes `--no-sequentialize-threads --no-deterministic-io`.
- T3 (covers F4): `dup(-1)` and `dup` of a closed descriptor give `EBADF`, identically in both runs of `--verify`.
  Existing candidate: `ATTEMPT("dup-badfd", dup(-1))` at `tests/c/errno_path_identity.c:105` (cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1787`, verify ci on, ptrace enabled).
- T4 (covers F5): lower `RLIMIT_NOFILE` with `setrlimit`, fill every free descriptor, and check that `dup` gives `EMFILE`, then succeeds after one `close`.
- T5 (covers F6): `dup` an `O_PATH` descriptor, a pipe end, a socket, an epoll, eventfd, timerfd, signalfd and memfd descriptor; on each copy, `fcntl(F_GETFL)` must succeed and `close` must succeed. This finds any descriptor kind that is missing from Detcore's table.
  Existing partial coverage: `dup` of an epoll descriptor in `tests/c/record_replay_fd_close.c:135` (cell `c-programs/record-replay-fd-close`, `tests/e2e/manifests/c-programs.yaml:6914`, verify ci on ptrace).
- T6 (covers F1, F3): record and replay a guest that duplicates a file descriptor, writes through both copies and closes one. Replay must return the same numbers and leave the same file contents.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel allocates the descriptor; Detcore copies its DetFd record with cleared descriptor flags and a shared open-file identity); linux_determinism stays conditional; 6 behaviours and 6 tests listed; the EBADF-after-kernel-success leak for unregistered descriptors is a code-reading hypothesis; no coverage token recorded, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `dup`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+dup&body=Discussion+for+the+%60dup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60dup%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `dup`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
