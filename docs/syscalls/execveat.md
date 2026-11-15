---
schema: syscall-pages/v1
syscall: execveat
nr_x86_64: 322
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+execveat&body=Discussion+for+the+%60execveat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexecveat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60execveat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 7
---

## Summary

`execveat(2)` is `execve(2)` with the program named relative to a directory descriptor, or by an open descriptor itself with `AT_EMPTY_PATH`. glibc's `fexecve` uses the `AT_EMPTY_PATH` form. Hermit handles it with the same Detcore code as `execve`, passing the directory descriptor, path and flags to the kernel unchanged. The flag `AT_EXECVE_CHECK` (Linux 6.14) appears to crash Detcore, because the code assumes that a successful exec never returns (see Hermit design).

## Linux semantics

The kernel resolves the program from `dirfd`, `pathname` and `flags`, and then behaves exactly like `execve`. On success the image is replaced and the call does not return; on failure it returns an error and the old image continues. `AT_EXECVE_CHECK` is the exception: it only checks whether the file may be executed, and returns 0 without replacing anything. Errors specific to this call: `EBADF` (an invalid `dirfd`), `ENOTDIR` (`dirfd` is not a directory and the path is relative), `ELOOP` (`AT_SYMLINK_NOFOLLOW` on a symbolic link), `ENOENT` (an empty path without `AT_EMPTY_PATH`, or a script run through a close-on-exec descriptor) and `EINVAL` (unknown flags).

### Flags and options

- F1: absolute `pathname`: `dirfd` is ignored, and the call behaves like `execve`.
- F2: relative `pathname` with `dirfd` set to `AT_FDCWD`: resolved against the current directory.
- F3: relative `pathname` with a directory descriptor, including one opened with `O_PATH`: resolved against that directory. A descriptor that is not a directory gives `ENOTDIR`; an invalid one gives `EBADF`.
- F4: `AT_EMPTY_PATH` with an empty `pathname`: executes the file that `dirfd` refers to. An empty `pathname` without the flag gives `ENOENT`.
- F5: `AT_SYMLINK_NOFOLLOW`: a symbolic link as the final component gives `ELOOP`.
- F6: `AT_EXECVE_CHECK` (Linux 6.14): checks execute permission, mount `noexec` and security-module policy, and returns 0 without executing. Hermit's target does not offer the check: it refuses the flag as F11 says.
- F7: unknown flag bits give `EINVAL`, and the old image continues.
- F8: descriptor-based execution of a script: the interpreter receives `/dev/fd/<N>` or `/dev/fd/<N>/<path>` as the script's name. If the descriptor is close-on-exec, the interpreter could not open it, so the call fails with `ENOENT`.
- F9: everything shared with `execve`: image replacement, closing close-on-exec descriptors, failure rollback, exec from a thread that is not the leader, signal and timer resets, `AT_RANDOM`, robust list and `clear_child_tid`. The `execve` body lists these as its F1-F11.
- F10: on a host kernel older than 6.14, `AT_EXECVE_CHECK` is an unknown flag and gives `EINVAL`. Under the target Hermit answers F11 before the kernel sees the call, so this host difference no longer reaches the guest.
- F11: `AT_EXECVE_CHECK` refused (owner decision 10): any call with the flag set returns `ENOSYS`, whatever the other arguments and flag bits, before anything is prepared for the exec. The old image continues with its descriptor table, memory model, robust list and ports unchanged, and the tracer does not stop.

## Determinism analysis

Path resolution relative to a descriptor is a pure function of the file system and the descriptor table, so F1-F5, F7 and F8 repeat when those repeat. Whether `AT_EXECVE_CHECK` is recognized depends on the host kernel version (F10). The shared exec behaviour (F9) carries the same sources as `execve`: fresh `AT_RANDOM` bytes, address-space layout randomization, and timing when a thread that is not the leader execs and kills its siblings. `linux_determinism` is `conditional`, matching `execve`.

### Cross-host variation

- Host kernel version: whether `AT_EXECVE_CHECK` performs the check (F6, 6.14 and later) or gives `EINVAL` (F10).

## Hermit design

`target_policy` is `model`, the same as `execve`: the kernel resolves and loads the program, and Detcore keeps its process model consistent around the call. Code at hermit f4055eb9cae4:

- Dispatch: `detcore/src/lib.rs:2747` runs `handle_execveat` (`detcore/src/syscalls/threads.rs:1443-1535`). `execve` converts to `execveat` and uses the same handler (`lib.rs:2746`). `lib.rs:1216` subscribes the syscall.
- F1-F5, F7, F8: `handle_execveat` never reads `dirfd`, `pathname` or `flags`; it uses the call only for its syscall number (`threads.rs:1527`, `threads.rs:1530`). Resolution and the errors listed above are the kernel's.
- F7 and F3/F4/F5 failures: these take the normal failure path. `threads.rs:1502` gets the error, and `threads.rs:1504-1516` restores the descriptor table, memory model, address-space identity, robust list and released ports, and calls `cancel_exec`.
- F9: the `execve` body's Hermit design applies unchanged: `prepare_exec` at `threads.rs:1471-1480`, the descriptor table built by `for_exec` at `threads.rs:1459-1467`, and the post-exec work in `handle_post_exec` (`lib.rs:1735-1805`).
- F6, guest-reachable panic (found by reading the code, not measured; the host kernel here is 7.1.3, `AT_EXECVE_CHECK` is `0x10000` in `/usr/include/linux/fcntl.h`, and the tree at this SHA never names it). A successful check returns 0 without replacing the image. `threads.rs:1502` calls `self.record_or_replay(guest, call).await.unwrap_err()`, which panics on that 0 after `prepare_exec` (`threads.rs:1471-1480`) has run and without `cancel_exec`. Under `hermit record`, the flag is outside the recorder's accepted set, so the exec is marked unrepresentable, is injected anyway, and `hermit-cli/src/recorder.rs:949` (`result.expect_err("successful exec unexpectedly returned to syscall handler")`) panics the same way.
- Target (owner decision 10): refuse `AT_EXECVE_CHECK` with `ENOSYS` (F11). Detcore's `handle_execveat` tests the flag before it builds the exec descriptor table (`threads.rs:1459-1467`) and before `prepare_exec`, and returns `ENOSYS` without forwarding; the recorder's `handle_exec` (`hermit-cli/src/recorder.rs:924`) does the same before `prepare_exec`, and records the error. Per owner decision 6 a refused form keeps its one errno before any argument check, so the flag wins over unknown flag bits and bad descriptors. Few programs use the flag yet, and every caller must already cope with its absence, because kernels before 6.14 reject it (F10). Those kernels answer `EINVAL`, not `ENOSYS`; the owner chose `ENOSYS`.
- Record and replay go through `hermit-cli` instead.
  - The recorder's `prepare_exec` (`hermit-cli/src/recorder.rs:959` onward) resolves the program itself so that the recording can name it. It accepts only `AT_EMPTY_PATH` and `AT_SYMLINK_NOFOLLOW` (`hermit-cli/src/recorder.rs:980-985`), and returns `ENOENT` for an empty path without `AT_EMPTY_PATH` (`hermit-cli/src/recorder.rs:988-990`).
  - An exec it cannot represent is still run. If it succeeds, `handle_post_exec` logs "recording cannot represent successful exec" and returns `EIO` (`hermit-cli/src/recorder.rs:653-672`).
  - The replayer routes `execveat` to the same exec handler (`hermit-cli/src/replayer.rs:392`).

## Recommended tests

- T1 (covers F1, F2): `execveat(AT_FDCWD, "/bin/true", ...)` with flags 0, and a relative path from the current directory. Output and exit status must match across runs under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/dbt_execveat_unsupported.c` (cell `c-programs/dbt-execveat-unsupported` at `tests/e2e/manifests/c-programs.yaml:855`, verify ci on ptrace). Its `ENOSYS` assertions are for DBT; on ptrace the absolute-path exec succeeds and replaces the image, so the cell checks only that this exec is deterministic.
- T2 (covers F3, F4, F5): an exec relative to an `O_PATH | O_DIRECTORY` descriptor and to a descriptor for a symbolic link to a directory; `AT_EMPTY_PATH` on an open program descriptor, checking that descriptor flags and a `flock` survive; `AT_SYMLINK_NOFOLLOW` on a link (`ELOOP`); exec through the `/proc/self/fd` and `/dev/fd` aliases. Add `ENOTDIR` (a regular-file `dirfd`), `EBADF` (a closed `dirfd`) and an empty path without `AT_EMPTY_PATH` (`ENOENT`).
  Existing candidate: `tests/c/record_replay_execveat_paths.c`, which covers all of the first sentence. It runs only through the record/replay test `record_c_execveat_paths` (`hermit-cli/tests/record_replay.rs:2318`, generated by the `record_replay_tests!` macro) and has no `hermit run --verify` cell.
- T3 (covers F4, F8): `fexecve` a `#!/bin/sh` script through a descriptor without close-on-exec (the script prints `$0`, which must be `/dev/fd/<N>` in every run), and through a close-on-exec descriptor (`ENOENT`, after which the program continues).
- T4 (covers F6, F11): `AT_EXECVE_CHECK` on an executable file, on a file without the execute bit, and together with an unknown flag bit and with a closed `dirfd`. Under `hermit run --verify` on ptrace and under `hermit record`, assert `ENOSYS` for every case, that the program continues, that its descriptors and a registered robust list are unchanged, that a following plain `execveat` succeeds, and that Hermit's exit status is the guest's, not 125. Natively on 6.14 or later the first two give 0 and `EACCES`, which shows what is refused. Today the first case is expected to hit the panic at `threads.rs:1502`.
- T5 (covers F7): an unknown flag bit gives `EINVAL`. Afterwards, descriptors, a registered robust list and the virtual clock are unchanged, and a following successful `execveat` works.
- T6 (covers F9): repeat the `execve` body's tests T1-T11 with `execveat(AT_FDCWD, path, argv, envp, 0)` in place of `execve`, or point those tests at both calls.
- U1 (covers F10; host-config): the CI host runs a kernel newer than 6.14, so it cannot produce this `EINVAL`. Detcore passes the flags to the kernel unchanged (`detcore/src/syscalls/threads.rs:1502`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (shares the execve handler; dirfd, path and flags go to the kernel unchanged); linux_determinism stays conditional; 9 flag behaviours and 6 tests listed, shared exec behaviour deferred to the execve body; suspected defect, found by reading the code: a successful AT_EXECVE_CHECK returns 0 into code that unwraps an error; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host kernel version decides whether AT_EXECVE_CHECK checks or gives EINVAL); F6 narrowed and new F10, pre-6.14 EINVAL (host-config U1); no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 10 and 6 applied (AT_EXECVE_CHECK refused with ENOSYS before any argument check, in Detcore and in the recorder, replacing the panics at threads.rs:1502 and recorder.rs:949); target_policy stays model; F6 and F10 narrowed; new F11; T4 now covers F6 and F11 and asserts ENOSYS; removed the AT_EXECVE_CHECK question, and with it the Open questions heading


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `execveat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+execveat&body=Discussion+for+the+%60execveat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexecveat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60execveat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `execveat`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The shared behaviour of the exec handler is described in the `execve` body (`ignored/syscall-tracking/proposed/execve.md`); this body covers only what `execveat` adds.

`hermit-cli/tests/fixtures/kvm_nonleader_exec.c` names `execveat` but drives the KVM refusal of an exec from a thread that is not the leader (`detcore/src/syscalls/threads.rs:1521-1532`). That is not ptrace behaviour, so it is not a test candidate here.
