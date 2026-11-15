---
schema: syscall-pages/v1
syscall: pidfd_getfd
nr_x86_64: 438
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/318
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pidfd_getfd&body=Discussion+for+the+%60pidfd_getfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpidfd_getfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pidfd_getfd%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`pidfd_getfd(pidfd, targetfd, flags)` duplicates descriptor `targetfd` of the process named by `pidfd` into the caller. Hermit models the self-targeted case: when the pidfd names the calling thread-group leader, Detcore installs the result as a real alias of the source open file. Every other target returns `EOPNOTSUPP`, and so does every call when threads are not sequentialized.

## Linux semantics

Added in Linux 5.6. The kernel checks `flags` first (any nonzero value is `EINVAL`), resolves the pidfd, checks that the caller may `ptrace`-attach to the target (`PTRACE_MODE_ATTACH_REALCREDS`), and then installs a new descriptor in the caller that refers to the same open file description as the target's `targetfd`: the two share the file offset, status flags and `flock` locks. The new descriptor is the lowest free number and always has `FD_CLOEXEC` set.

### Flags and options

- F1: `flags == 0`, live target, open `targetfd`: returns the lowest free descriptor in the caller, with `FD_CLOEXEC`, aliasing the target's open file description.
- F2: `flags != 0`: returns `EINVAL`, ahead of every descriptor or permission check.
- F3: `targetfd` not open in the target: returns `EBADF`.
- F4: `pidfd` is not a pidfd, or is not open: returns `EBADF`.
- F5: Target errors: an exited target returns `ESRCH`; a target the caller may not ptrace-attach to returns `EPERM`.
- F6: Target identity: the target may be the caller itself, another process, or another thread group member with a different descriptor table.
- F7: Caller's table full (`RLIMIT_NOFILE` reached): returns `EMFILE`.

## Determinism analysis

`linux_determinism: conditional`. The result depends on two descriptor tables, the caller's and the target's, and on the target being alive.

### Nondeterminism sources

- The returned number is the lowest free descriptor in the caller's table, which depends on what the caller and threads that share its table have opened.
- For another process, which file `targetfd` names at the moment of the call, or whether it is open at all, depends on how far the target has run.
- `ESRCH` versus success depends on whether the target has exited, which is a race.

The call repeats when both tables are fixed and the target's progress relative to the caller is fixed by a deterministic scheduler; the self-targeted case needs only the caller's own table.

## Hermit design

Target policy: `model`. Detcore owns the descriptor table, so the duplicate must be installed in its model as an alias of the source open file; it does that for the self-targeted case and refuses the rest.

What the code does at hermit f4055eb9cae4:

- Dispatch: a Sysno-guarded arm passes `pidfd`, `targetfd` and `flags` to `handle_pidfd_getfd` (`detcore/src/lib.rs:2068-2082`); the call stays subscribed under the pass-through optimization (`detcore/src/lib.rs:1189-1192`).
- F2: nonzero `flags` takes the raw `record_or_replay` path "to preserve the kernel's exact `EINVAL`"; an unexpected success is closed and reported as a Tool error (`detcore/src/syscalls/files.rs:4411-4413`, `detcore/src/syscalls/files.rs:4432-4443`).
- Without `sequentialize_threads`, the call is refused with `EOPNOTSUPP` before reaching the kernel (`detcore/src/syscalls/files.rs:4445-4449`).
- Source capture: Detcore reads the caller's TGID and TID (`detcore/src/syscalls/files.rs:4451-4452`) and validates both descriptors under one table lock (`detcore/src/tool_local.rs:2640-2669`): a descriptor not modeled as `FdType::Pidfd` returns `EBADF` (`detcore/src/tool_local.rs:2659-2661`), and a target that is not the calling task returns `EOPNOTSUPP` (`detcore/src/tool_local.rs:2662-2664`). The doc comment explains why: "`CLONE_THREAD` does not imply `CLONE_FILES`", so only `target == getpid() == gettid()` proves `targetfd` is in the caller's table (`detcore/src/syscalls/files.rs:4415-4423`).
- Success: the kernel call goes through `record_or_replay`; on failure the capture is abandoned (`detcore/src/syscalls/files.rs:4459-4467`); on success the result is installed as an alias with `O_CLOEXEC` (`detcore/src/syscalls/files.rs:4468-4473`). If the source table changed in between, the new descriptor is closed and the call returns `EOPNOTSUPP` (`detcore/src/syscalls/files.rs:4475-4494`).

Why model: the legacy recommendation was NEEDS-HANDLER, and the handler exists. The self-targeted alias is fully modeled, including its effect on Detcore's `flock` state. A cross-process target would need a channel to keep another task's cached descriptor state current, which Detcore does not have, so it fails closed.

Known gaps:

- F5 and the cross-process half of F6 return `EOPNOTSUPP` under Hermit, where Linux returns a descriptor, `ESRCH` or `EPERM`.
- The relaxed (`--no-sequentialize-threads`) refusal also returns `EOPNOTSUPP` for arguments Linux would reject with `EBADF`, by design (`hermit-cli/tests/flock_exclusion.rs:534-552`).
- `tests/c/flock_exclusion.c` has no e2e manifest cell; it runs only through the cargo tests below.

## Recommended tests

- T1 (covers F1, F2, F3, F6): Self dup of a locked descriptor shares `flock` state with its source; `targetfd = -1` returns `EBADF`; four nonzero-flags combinations return `EINVAL`; a foreign child's descriptor returns `EOPNOTSUPP`. Existing candidate: the `#[test]` `pidfd_getfd_alias_mutation_invalidates_source_flock_authority` in `hermit-cli/tests/flock_exclusion.rs` (lines 529-532, ptrace, scenario markers at lines 502-514), driving `tests/c/flock_exclusion.c` (self dup at line 656, `EBADF` at line 708, flags cases at lines 729-763, foreign refusal at lines 854-859). It runs with `--allow-unsupported-syscalls` (line 494).
- T2 (covers F1, F2): Record and replay a successful self dup that shares a file offset with its source, plus nonzero-flags failures. Existing candidate: the `#[test]` `replay_reissues_pidfd_getfd_success_and_failure` in `hermit-cli/tests/flock_exclusion.rs` (line 849), scenario `pidfd-getfd-record`.
- T3 (covers F4): With `flags == 0`, a regular-file descriptor, a closed descriptor and `-1` as `pidfd` each return `EBADF` under `--strict --verify`, and the caller's table is unchanged.
- T4 (covers F5, F6): A non-leader thread calls with a pidfd for its own process and gets `EOPNOTSUPP`; a pidfd for a reaped child returns `EOPNOTSUPP` under Hermit (Linux: `ESRCH`); both repeat under `--verify`.
- T5 (covers F7): Lower `RLIMIT_NOFILE`, fill the table, and a self `pidfd_getfd` returns `EMFILE`; the source descriptor and its lock remain usable afterwards.
- T6 (covers none): With `--no-sequentialize-threads`, `pidfd_getfd(-1, -1, 0)` returns `EOPNOTSUPP` without reaching the kernel. Existing candidate: the `#[test]` `pidfd_getfd_relaxed_mode_refuses_before_kernel_injection` in `hermit-cli/tests/flock_exclusion.rs` (lines 534-552).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/318#issuecomment-5872901827
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/318: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch sysno-guard.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pidfd_getfd records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/318 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (self-targeted alias installed in Detcore's table; nonzero flags forwarded for kernel EINVAL; foreign targets and unsequentialized runs refused with EOPNOTSUPP); linux_determinism stays conditional; 7 flag behaviours and 6 tests listed; T1, T2 and T6 have ptrace cargo-test candidates but F4, F5 and F7 are uncovered, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pidfd_getfd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pidfd_getfd&body=Discussion+for+the+%60pidfd_getfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpidfd_getfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pidfd_getfd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pidfd_getfd" (https://github.com/rrnewton/hermit/issues/318), legacy variant A1.

The legacy Determinism analysis said the returned descriptor and the pidfd identity "require virtualization". At f4055eb9cae4 the descriptor number comes from the kernel through record/replay, and the pidfd's target is recorded at `pidfd_open` time; Detcore does not renumber either.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/318.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pidfd_getfd`, and by confirming hermit currently routes `pidfd_getfd` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pidfd_getfd(3, 0, 0)            = 4
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/318#issuecomment-5872901827
