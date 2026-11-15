---
schema: syscall-pages/v1
syscall: seccomp
nr_x86_64: 317
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/420
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+seccomp&body=Discussion+for+the+%60seccomp%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fseccomp.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60seccomp%60%2C+or+in+what+the+page+says%3F%0A
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

`seccomp(operation, flags, args)` installs a syscall filter on the calling thread (strict mode or a classic BPF program) or queries what the kernel's seccomp support offers. Detcore handles it (`detcore/src/lib.rs:2519`, `detcore/src/syscalls/misc.rs:262`) without ever installing a filter: it reproduces a subset of the kernel's argument checks and answers every otherwise-valid operation with `EOPNOTSUPP` (`detcore/src/syscalls/misc.rs:58-79`). Owner decision 9 sets the target to `refuse`, and decision 6 makes the refusal one fixed errno returned before any argument check; this body chooses `ENOSYS`, the answer of a kernel built without `CONFIG_SECCOMP`, so today's code has to change.

Manual reference: Linux man-pages seccomp(2), seccomp_unotify(2).

## Linux semantics

A filter applies to every later syscall of the thread that installed it, is inherited by children across `fork`/`clone`, and survives `execve`. Filters stack: each one runs on every syscall and the action with the highest precedence wins (`KILL_PROCESS`, `KILL_THREAD`, `TRAP`, `ERRNO`, `USER_NOTIF`, `TRACE`, `LOG`, `ALLOW`, in that order). A filter can never be removed. Installing one requires `no_new_privs` (set with `prctl(PR_SET_NO_NEW_PRIVS)`) or `CAP_SYS_ADMIN`, otherwise `EACCES`.

The kernel checks the operation first (`EINVAL` for an unknown one), then the flags valid for that operation (`EINVAL`), then reads `args`, where a NULL or unreadable pointer is `EFAULT`.

`prctl(PR_SET_SECCOMP)` and `prctl(PR_GET_SECCOMP)` are older entry points to the same state; they are prctl options and belong to the `prctl` body.

### Flags and options

- F1: `SECCOMP_SET_MODE_STRICT` (0) with `flags` 0 and `args` NULL: the thread may afterwards call only `read`, `write`, `_exit` and `rt_sigreturn`; any other syscall kills it with `SIGKILL`.
- F2: `SECCOMP_SET_MODE_STRICT` with nonzero `flags` or non-NULL `args`: `EINVAL`.
- F3: `SECCOMP_SET_MODE_FILTER` (1) with `flags` 0: install the `struct sock_fprog` in `args`. A malformed program is `EINVAL`, an over-long one `ENOMEM`, and a caller without `no_new_privs` or `CAP_SYS_ADMIN` gets `EACCES`.
- F4: `SECCOMP_FILTER_FLAG_TSYNC` (1): install the filter on every thread of the process. If some thread cannot be synchronized, the call returns that thread's tid.
- F5: `SECCOMP_FILTER_FLAG_LOG` (2): log every action except `ALLOW`.
- F6: `SECCOMP_FILTER_FLAG_SPEC_ALLOW` (4): do not enable the speculative store bypass mitigation.
- F7: `SECCOMP_FILTER_FLAG_NEW_LISTENER` (8): return a user-notification file descriptor through which a supervisor answers `SECCOMP_RET_USER_NOTIF` syscalls.
- F8: `SECCOMP_FILTER_FLAG_TSYNC_ESRCH` (16): with TSYNC, report a synchronization failure as `ESRCH` instead of a tid; required to combine TSYNC with NEW_LISTENER.
- F9: `SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV` (32): with NEW_LISTENER, wait killably once the supervisor has received the notification.
- F10: an undefined filter flag, or NEW_LISTENER combined with TSYNC without TSYNC_ESRCH: `EINVAL`.
- F11: `SECCOMP_GET_ACTION_AVAIL` (2): `flags` must be 0 (`EINVAL`); `args` points to a `u32` action, and the call returns 0 when the kernel supports it and `EOPNOTSUPP` when not.
- F12: `SECCOMP_GET_NOTIF_SIZES` (3): `flags` must be 0 (`EINVAL`); writes `struct seccomp_notif_sizes` to `args` and returns 0.
- F13: an operation above 3: `EINVAL`.
- F14: `args` NULL for `SECCOMP_SET_MODE_FILTER`, `SECCOMP_GET_ACTION_AVAIL` or `SECCOMP_GET_NOTIF_SIZES`, or a non-NULL pointer that cannot be accessed: `EFAULT`.
- F15: enforcement of an installed filter on later syscalls (the actions listed above), its inheritance across `fork`/`clone`, and its persistence across `execve`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The argument checks (F2, F10, F13, F14) are pure functions of the arguments. Whether an installation succeeds depends on the caller's privilege and `no_new_privs` state (F3), the answer to F11 depends on the kernel version, and TSYNC's result depends on the state of the other threads at that moment (F4, F8). Once installed, a classic BPF filter is a pure function of the syscall number, architecture, instruction pointer and argument registers, so enforcement (F15) repeats for a repeated syscall sequence. The exceptions are `USER_NOTIF` (F7, F9), whose answers come from a supervisor process with its own timing, and `LOG` (F5), which writes to the host audit log.

### Nondeterminism sources

- Caller privilege and `no_new_privs` state, which decide `EACCES`
- Kernel version and configuration, which decide which actions and flags exist
- Timing of other threads during a TSYNC installation
- A user-notification supervisor's timing and answers
- Instruction-pointer-dependent filters, which repeat only if the guest's memory layout repeats

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_SECCOMP`, returned before any argument is examined, so a valid filter installation, a valid query and a garbage-argument call all get the same `ENOSYS`. Linux's `do_seccomp` checks in this order: the operation switch, where an unknown operation gives `EINVAL` (F13); then per operation, strict mode rejects nonzero `flags` or `args` with `EINVAL` (F2); `seccomp_set_mode_filter` rejects undefined flags and NEW_LISTENER with TSYNC but without TSYNC_ESRCH with `EINVAL` (F10), then copies the program (`EFAULT`, F14), then checks `no_new_privs` or `CAP_SYS_ADMIN` (`EACCES`, F3); GET_ACTION_AVAIL and GET_NOTIF_SIZES reject nonzero `flags` with `EINVAL` and a bad pointer with `EFAULT` (F11, F12, F14). The fixed `ENOSYS` comes before all of these. The errno is a judgement call. Today's handler answers otherwise-valid calls with `EOPNOTSUPP` (`detcore/src/syscalls/misc.rs:76-78`), but Linux never returns `EOPNOTSUPP` from strict mode or filter installation, only from GET_ACTION_AVAIL for an unknown action, so under the standing least-common-denominator policy it is not a configuration a guest already handles. `ENOSYS` is, and it matches Detcore's refusal of the sibling sandbox interface, Landlock, which returns `ENOSYS` as "a kernel built without Landlock" (`detcore/src/lib.rs:2316-2323`). A kernel without seccomp refuses every caller, so decision 6's unprivileged-success exception does not apply, even though strict mode and filter installation under `no_new_privs` are unprivileged on Linux. The supportable fallback is a program that runs without its filter when seccomp is unavailable: OpenSSH's privilege-separation sandbox logs the failed filter installation and continues (recalled, not re-checked in the OpenSSH source for this body); programs that make seccomp mandatory, such as Chromium's sandbox without `--no-sandbox`, will not run. Owner decision 9 rules out evaluating guest filters inside Detcore. Code change needed: `seccomp_result` (`detcore/src/syscalls/misc.rs:58-79`) must drop its `EINVAL` and `EFAULT` checks (`:59-74`) and return `ENOSYS` for every call, and `seccomp` must join a refusal predicate in `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`) so that `code_facts.refused` becomes true, which L14 requires before this body can be `done`. The `EOPNOTSUPP` assertion in `tests/c/seccomp_refusal.c` and the unit test `seccomp_tsync_null_probe_matches_linux_validation` (`detcore/src/syscalls/misc.rs:1071`) change with it. Until then the bullets below describe today's code, not the target.

What `handle_seccomp` does (`detcore/src/syscalls/misc.rs:260-268`, logic in `seccomp_result` at `:58-79`):

- F13: an operation above 3 is `EINVAL` (`:59-61`), as on Linux.
- F2: strict mode with nonzero `flags` or non-NULL `args` is `EINVAL` (`:62-64`), as on Linux.
- F10 and F5-F9: filter mode with any flag other than TSYNC is `EINVAL` (`:65-67`). That is right for undefined bits, but LOG, SPEC_ALLOW, NEW_LISTENER, TSYNC_ESRCH and WAIT_KILLABLE_RECV are valid on Linux, and Hermit refuses them with `EINVAL` rather than with its usual `EOPNOTSUPP`.
- F14: a NULL `args` for filter mode, GET_ACTION_AVAIL or GET_NOTIF_SIZES is `EFAULT` (`:68-74`). A non-NULL pointer is never read, so an unreadable one gets `EOPNOTSUPP`, not `EFAULT`.
- F1, F3, F4, F11, F12: every other call is `EOPNOTSUPP` (`:76-78`). The comment there gives the reason: "Hermit cannot enforce a guest-installed BPF policy in every backend." Strict mode is not entered, no filter is installed (so F15 never happens), GET_ACTION_AVAIL reports even `SECCOMP_RET_ALLOW` as unsupported, and GET_NOTIF_SIZES writes nothing. The `flags` argument of GET_ACTION_AVAIL and GET_NOTIF_SIZES is never checked, so a nonzero value gets `EOPNOTSUPP` where Linux gives `EINVAL`.

Related paths: `prctl(PR_GET_SECCOMP)`, `prctl(PR_SET_SECCOMP)` and `prctl(PR_SET_NO_NEW_PRIVS)` are not in Detcore's supported prctl list (`detcore/src/syscalls/misc.rs:81`, with the `PR_SET_NO_NEW_PRIVS` exclusion asserted at `:967`), so they fall to `ENOSYS` (`:403`). A program that follows the usual sequence, `PR_SET_NO_NEW_PRIVS` and then `seccomp`, therefore fails at the first step.

The answer is deterministic and the same in every backend, which is why the code facts record the call as determinized rather than refused. The code tag `TODO-HUMAN-REVIEW(PR-874)` at `detcore/src/syscalls/misc.rs:261` (https://github.com/rrnewton/hermit/pull/874) asks for human review of this validation.

The policies considered before owner decision 9 (kept for the reasoning; decision 9 chose `refuse`):

- `emulate` the unsupported answer, as today. The guest never gets a filter, and the fixed errnos are part of the contract that `tests/c/seccomp_refusal.c` states in its header comment. Sandboxing programs (OpenSSH's privilege-separated child, systemd units, Chromium, Firefox, libseccomp users) either fall back to running without a filter or abort.
- `refuse`, formally. The behaviour would barely change, but L14 counts `refuse` as realized only when the code facts record the call as refused, and this handler returns its answer as a normal determinized result. This is the policy decision 9 chose, so the code change above includes the refusal predicate.
- `model`, by evaluating the guest's classic BPF program inside Detcore on every intercepted syscall. A classic BPF filter sees only `struct seccomp_data` (number, architecture, instruction pointer, arguments), which Detcore already has, so the evaluation is deterministic and independent of the backend. It would need Detcore to implement each action, including `TRAP` as a `SIGSYS` delivered through Detcore's signal scheduling, and to decide what `USER_NOTIF` and `TRACE` mean inside a sandbox.
- `passthrough` on ptrace only, letting the kernel stack the guest's filter on top of the one Reverie installs to trap syscalls (`reverie-ptrace/src/backend.rs:25` and `:63` in the pinned Reverie submodule d766df20). Under the precedence rule, a guest `ERRNO`, `TRAP` or `KILL` action would outrank Reverie's `TRACE`, so those syscalls would never reach Detcore, and a guest `TRACE` action would be delivered to Reverie as if it were its own stop. This is why the handler's comment refers to backends.

## Recommended tests

- T1 (covers F2, F10, F13, F14): with raw `syscall(SYS_seccomp, ...)` under `hermit run --verify` on ptrace, operation 4, strict mode with `flags` 1 or a non-NULL `args`, filter mode with flag bit 31, and filter mode, GET_ACTION_AVAIL and GET_NOTIF_SIZES with NULL `args` or with `(void *)-1` all return -1 with `errno == ENOSYS`, never `EINVAL` or `EFAULT`, which shows the fixed errno comes before every argument check.
  Existing partial coverage: the unit test `seccomp_tsync_null_probe_matches_linux_validation` at `detcore/src/syscalls/misc.rs:1071` checks filter mode with TSYNC and NULL `args`, with flag bit 31, and with a non-NULL `args`, without running a guest; it asserts today's Linux-style `EFAULT` and `EINVAL`, so it must be rewritten to assert `ENOSYS`.
- T2 (covers F11, F12): GET_ACTION_AVAIL for `SECCOMP_RET_ALLOW` with a valid pointer and with `flags` 1, and GET_NOTIF_SIZES with a valid buffer and with `flags` 1, all under `hermit run --verify` on ptrace. Each returns -1 with `errno == ENOSYS`, and GET_NOTIF_SIZES leaves the buffer untouched.
  Existing candidates: `tests/c/seccomp_refusal.c` (cell `c-programs/seccomp-refusal`, `tests/e2e/manifests/c-programs.yaml:13594`) and `tests/c/syscall_quick_wins.c:121-127` (cell `c-programs/syscall-quick-wins`, `tests/e2e/manifests/c-programs.yaml:9345`), both verify cells with ptrace in CI. Both assert today's `EOPNOTSUPP`, so they change to `ENOSYS` with the code, and neither checks the nonzero-`flags` case.
- T3 (covers F1, F3, F4): strict mode with valid arguments, and filter mode with `flags` 0 and with TSYNC on a valid one-instruction `ALLOW` program. Each must return -1 with `errno == ENOSYS` (`EOPNOTSUPP` today), and afterwards the guest must still be able to call `getpid` and `open`, proving that no filter or strict mode took effect.
- T4 (covers F5, F6, F7, F8, F9): filter mode with each of LOG, SPEC_ALLOW, NEW_LISTENER, TSYNC|NEW_LISTENER|TSYNC_ESRCH, and NEW_LISTENER|WAIT_KILLABLE_RECV on a valid program. Each must return -1 with `errno == ENOSYS`; today they get `EINVAL` (`detcore/src/syscalls/misc.rs:65-67`).
- T5 (covers F15): try to install an `ERRNO(EPERM)` filter for `getpid` after `prctl(PR_SET_NO_NEW_PRIVS)`. The installation returns -1 with `errno == ENOSYS`, and `getpid` keeps working in the caller, in a forked child and after `execve`, identically across `--verify` runs.
  Not a candidate: `hermit-cli/tests/cli.rs` also names seccomp, but its `deny_syscall` helper (`hermit-cli/tests/cli.rs:1224-1273`) and `run_reports_denied_ptrace_and_seccomp_capabilities` (`hermit-cli/tests/cli.rs:5436`) put a filter on Hermit itself to test its capability reporting. They do not exercise the guest's `seccomp` calls.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/420: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy stays investigate (code answers every valid operation with EOPNOTSUPP and never installs a filter, misc.rs:58-79; owner must choose between keeping that answer and supporting guest filters); linux_determinism stays conditional; 15 flag behaviours and 5 tests listed; divergences recorded (valid filter flags get EINVAL, query flags unchecked, non-NULL args never read); no coverage token recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_SECCOMP, before any argument check; needs a code change at detcore/src/syscalls/misc.rs:58-79 and a refusal predicate for L14); target_policy investigate -> refuse; no new F/T ids, T1-T5 now assert ENOSYS; removed the open questions on which policy to commit to and on the flag errnos; added an open question on ENOSYS versus EOPNOTSUPP

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `seccomp`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+seccomp&body=Discussion+for+the+%60seccomp%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fseccomp.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60seccomp%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: seccomp" (https://github.com/rrnewton/hermit/issues/420), legacy variant A1.

### Open questions

- The target errno is `ENOSYS` (kernel without `CONFIG_SECCOMP`) rather than today's `EOPNOTSUPP`, chosen under the standing policy; the owner may prefer to keep `EOPNOTSUPP`, which the existing fixtures assert. Either way decision 6 removes today's `EINVAL` and `EFAULT` checks.
- `prctl(PR_SET_NO_NEW_PRIVS)` falls to `ENOSYS` (`detcore/src/syscalls/misc.rs:403`). It is the usual first step before `seccomp`, and it has uses of its own (for example `setpriv --no-new-privs` and systemd's `NoNewPrivileges=`). That belongs to the `prctl` body, but it limits what any `seccomp` policy can achieve.

### 2026-07-24 host strace evidence

This section is history. At hermit f4055eb9cae4 the call is dispatched by its own typed arm (`detcore/src/lib.rs:2519`), not by the old classification pass-through it describes.

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=seccomp` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `seccomp` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
seccomp(0xffffffff /* SECCOMP_SET_MODE_??? */, 0, NULL) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
