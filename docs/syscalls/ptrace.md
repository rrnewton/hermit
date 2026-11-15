---
schema: syscall-pages/v1
syscall: ptrace
nr_x86_64: 101
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/385
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ptrace&body=Discussion+for+the+%60ptrace%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fptrace.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ptrace%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_privileged_observation_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`ptrace(request, pid, addr, data)` lets one process observe and control another: attach to it, read and write its memory and registers, resume or single-step it, and receive stops for its signals, syscalls and lifecycle events. Hermit refuses every request with a fixed `EPERM` and never enters the host kernel (`detcore/src/lib.rs:2188` at hermit f4055eb9cae4), so guests cannot trace other guest processes. Debuggers, `strace` and anything else that needs a tracer do not work inside Hermit.

Manual reference: Linux man-pages ptrace(2).

## Linux semantics

A tracee has at most one tracer. A process becomes a tracee by calling `PTRACE_TRACEME`, which makes its parent the tracer, or when a tracer uses `PTRACE_ATTACH` or `PTRACE_SEIZE` on it. Both fail with `EPERM` if the target is already traced. Attaching also needs `PTRACE_MODE_ATTACH_REALCREDS` access to the target, which credentials, `CAP_SYS_PTRACE`, Yama's `ptrace_scope` and other LSMs decide. A kernel thread and a thread of the caller's own thread group also give `EPERM`.

Most requests act only on a tracee of the caller that is currently in a ptrace-stop. Any other target gives `ESRCH`, whether the pid is a non-tracee, a tracee that is running, or a pid that does not exist. An unknown request on a stopped tracee gives `EIO`. A bad user pointer gives `EFAULT`, and an unaligned or out-of-range `PEEKUSER`/`POKEUSER` offset gives `EIO`.

The tracer learns of tracee stops through `waitpid`/`waitid`: signal-delivery stops, group stops, syscall-entry and syscall-exit stops, and `PTRACE_EVENT_*` stops. Each stop's ordering against the tracer's own execution and against other tracees depends on host scheduling. While attached, the tracer sees and can rewrite the tracee's registers and memory at each stop. The tracee's `/proc/<pid>/status` shows `TracerPid` as nonzero.

The raw syscall and the glibc wrapper differ for `PEEK*`: the raw syscall stores the word through `data` and returns 0, while the wrapper returns the word and makes callers check `errno`.

### Flags and options

- F1: `PTRACE_TRACEME`: make the caller a tracee of its parent. It fails with `EPERM` if the caller is already traced.
- F2: `PTRACE_ATTACH`: attach to `pid` and send it `SIGSTOP`. It fails with `EPERM` if the target is already traced, if access is denied, or if the target is a kernel thread or in the caller's own thread group, and with `ESRCH` if `pid` does not exist.
- F3: `PTRACE_SEIZE`: attach without stopping, with initial `PTRACE_O_*` options in `data`. It has the same permission rules as F2.
- F4: `PTRACE_INTERRUPT` and `PTRACE_LISTEN`: stop a seized tracee, and leave a group stop while still being notified. Both need a tracee that was attached with `PTRACE_SEIZE`.
- F5: `PTRACE_PEEKTEXT`, `PTRACE_PEEKDATA`, `PTRACE_PEEKUSER`: read a word of tracee memory or of the tracee's `struct user`. The raw syscall stores the word through `data`.
- F6: `PTRACE_POKETEXT`, `PTRACE_POKEDATA`, `PTRACE_POKEUSER`: write a word of tracee memory or of `struct user`, including the debug registers.
- F7: register access: `PTRACE_GETREGS`, `PTRACE_SETREGS`, `PTRACE_GETFPREGS`, `PTRACE_SETFPREGS`, `PTRACE_GETFPXREGS`, `PTRACE_SETFPXREGS`, and `PTRACE_GETREGSET`/`PTRACE_SETREGSET` with an `NT_*` note type (for example `NT_PRSTATUS`, `NT_PRFPREG`, `NT_X86_XSTATE`) and an `iovec`.
- F8: signal state: `PTRACE_GETSIGINFO`, `PTRACE_SETSIGINFO`, `PTRACE_PEEKSIGINFO` (with the `PTRACE_PEEKSIGINFO_SHARED` flag), `PTRACE_GETSIGMASK`, `PTRACE_SETSIGMASK`.
- F9: `PTRACE_SETOPTIONS` (and the legacy `PTRACE_OLDSETOPTIONS`) with any of `PTRACE_O_TRACESYSGOOD`, `PTRACE_O_TRACEFORK`, `PTRACE_O_TRACEVFORK`, `PTRACE_O_TRACECLONE`, `PTRACE_O_TRACEEXEC`, `PTRACE_O_TRACEVFORKDONE`, `PTRACE_O_TRACEEXIT`, `PTRACE_O_TRACESECCOMP`, `PTRACE_O_EXITKILL`, `PTRACE_O_SUSPEND_SECCOMP`. An undefined option bit gives `EINVAL`.
- F10: `PTRACE_GETEVENTMSG`: read the message of the last `PTRACE_EVENT_*` stop, for example the new pid after a fork event.
- F11: resume requests, each with an optional signal to deliver in `data`: `PTRACE_CONT`, `PTRACE_SYSCALL`, `PTRACE_SINGLESTEP`, `PTRACE_SINGLEBLOCK`, `PTRACE_SYSEMU`, `PTRACE_SYSEMU_SINGLESTEP`.
- F12: `PTRACE_KILL` (deprecated) and `PTRACE_DETACH`, which resumes the tracee and ends tracing, optionally delivering a signal.
- F13: x86 thread-area requests: `PTRACE_GET_THREAD_AREA`, `PTRACE_SET_THREAD_AREA`, `PTRACE_ARCH_PRCTL`.
- F14: introspection and configuration requests: `PTRACE_SECCOMP_GET_FILTER`, `PTRACE_SECCOMP_GET_METADATA`, `PTRACE_GET_SYSCALL_INFO`, `PTRACE_SET_SYSCALL_INFO`, `PTRACE_GET_RSEQ_CONFIGURATION`, `PTRACE_GET_SYSCALL_USER_DISPATCH_CONFIG`, `PTRACE_SET_SYSCALL_USER_DISPATCH_CONFIG`.
- F15: error selection: `ESRCH` when the target does not exist or is not a stopped tracee of the caller; `EPERM` for permission failures and an already-traced target; `EIO` for an unknown request or a bad `struct user` offset; `EFAULT` for a bad pointer; `EINVAL` for bad option bits.
- F16: stop reporting: once tracing is active, the tracer sees the tracee's stops through `waitpid`/`waitid` status values (`WIFSTOPPED`, `status >> 8` carrying `SIGTRAP | PTRACE_EVENT_* << 8` or `SIGTRAP | 0x80`), and `TracerPid` in the tracee's `/proc/<pid>/status` is nonzero.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Tracing exposes another process's live execution state. When each stop happens, the order of stops across several tracees, and the register and memory contents at each stop depend on host scheduling and signal timing. Whether an attach succeeds depends on host policy: credentials, capabilities, Yama `ptrace_scope` and LSMs. Only a few error results repeat, for example `PTRACE_TRACEME` in a process that is already traced.

### Nondeterminism sources

- Timing and ordering of tracee stops relative to the tracer and to each other
- Register and memory snapshots taken at those stops
- Host permission policy (Yama, LSMs, capabilities), which decides whether an attach succeeds
- Under Hermit's ptrace backend, every guest thread is already traced by Hermit, so a nested tracer would compete with Hermit's own instrumentation

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `EPERM`, returned before any argument is examined. Linux's `ptrace` checks in this order: `PTRACE_TRACEME` is handled first and gives `EPERM` when the caller is already traced; for every other request the PID lookup gives `ESRCH`; `PTRACE_ATTACH` and `PTRACE_SEIZE` then run the attach checks (`EPERM`, or `EIO` for bad seize flags); the remaining requests need a stopped tracee of the caller (`ESRCH`) before the request itself is decoded (`EIO` for an unknown request, `EFAULT` for a bad pointer). Under decision 6 the fixed `EPERM` comes before all of these, so a valid request and a garbage request get the same `EPERM`, as T3 and T5 assert; Linux's `ESRCH` for non-attach requests is not adopted. No unprivileged success case needs preserving on the ptrace backend, where every guest thread already has Hermit as its tracer and Linux itself refuses `PTRACE_TRACEME` and attach with `EPERM`. Owner decision 9 makes `refuse` the long-term target; Detcore will not emulate a tracer. The supportable fallback is a program that treats a failed `PTRACE_TRACEME` or attach as running under a debugger or in a restricted sandbox (Yama `ptrace_scope` 3 refuses every attach the same way); debuggers, `strace` and LeakSanitizer have no such fallback and do not run under Hermit. Today's code already realizes the target, so no code change is needed:

- The dispatch arm at `detcore/src/lib.rs:2188` matches `is_privileged_observation_refused_syscall` (`detcore/src/syscall_classification.rs:1155`, set `{ptrace, kcmp}` at `detcore/src/syscall_classification.rs:1159`) and returns `Err(Error::Errno(Errno::EPERM))` at `detcore/src/lib.rs:2191`. It comes before any host call, so every request (F1-F14) gets `EPERM` whatever its arguments. Nothing reaches the host, so run, `--verify`, record and replay all see the same result. The doc comment at `detcore/src/syscall_classification.rs:1151` gives the reason: nested `ptrace` depends on untranslated host PIDs, permissions and process lifetimes.
- The refusal is unconditional: `refused: true` and `refused_only_when_fail_closed: false`. The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1329`), and `ptrace` is in the Determinized list at `detcore/src/syscall_classification.rs:562`.
- The guest's view of `/proc/<pid>/status` agrees: `sanitize_status` writes `TracerPid:\t1` for a guest process with a virtual identity (`detcore/src/procfs.rs:1411`). A guest that checks for a debugger sees that it is traced, which matches the `EPERM` from `PTRACE_TRACEME`.

Why `refuse`:

- `EPERM` is a real Linux result. On the ptrace backend every guest thread already has Hermit as its tracer, and the kernel itself gives `EPERM` to `PTRACE_TRACEME` and to `PTRACE_ATTACH`/`PTRACE_SEIZE` of a traced target (F1-F3). Hermit makes that answer fixed and independent of host policy.
- `passthrough` is not possible without breaking Hermit: a guest attach would either fail on host state that varies, or reach processes outside Hermit's scheduler, with stops whose timing Hermit cannot control.
- `model` would mean Detcore emulating a tracer for guest tracees. It would have to own tracer-tracee relationships, make `waitpid` report stops at deterministic points, and serve register and memory access from its own view of the guest. rr does this for a subset of requests; nothing in Hermit at this SHA attempts it, and owner decision 9 rules it out.

Known differences from Linux under this policy:

- Requests that Linux answers with `ESRCH`, `EIO`, `EFAULT` or `EINVAL` (F4-F15: a non-tracee target, a missing pid, a bad request number, a bad pointer, bad option bits) all get `EPERM` instead. The result is still deterministic, but the errno differs.
- Programs that need a tracer fail inside Hermit: debuggers, `strace`, and runtimes that use ptrace internally (for example LeakSanitizer, which attaches to its own threads to suspend them). The util-c/pmu-skid cell (`tests/e2e/manifests/util-c.yaml:4`) expects such a program to exit with status 3 after `PTRACE_TRACEME` fails (`tests/util/pmu_skid.c:273`).
- No ptrace stop ever happens, so F16 is never observable for a guest tracee, apart from the fixed `TracerPid` above.

## Recommended tests

- T1 (covers F1, F16): `PTRACE_TRACEME` returns `-1`/`EPERM` under `hermit run --verify` on ptrace, and `/proc/self/status` reports `TracerPid:\t1` in the same guest.
  Existing candidate for the `EPERM` half: `tests/c/ptrace_traceme_eperm.c` (cell `c-programs/ptrace-traceme-eperm`, `tests/e2e/manifests/c-programs.yaml:6589`, verify ci on ptrace). Nothing checks `TracerPid` yet.
- T2 (covers F2, F3, F4): `PTRACE_ATTACH` to pid 1 and to a forked guest child, and `PTRACE_SEIZE`, `PTRACE_INTERRUPT` and `PTRACE_LISTEN` on that child, all return `EPERM`. The child is never stopped: it runs to completion and `waitpid` reports a normal exit.
  Existing candidates for `PTRACE_ATTACH` to pid 1: `tests/c/ptrace_eperm.c` (cell `c-programs/ptrace-eperm`, `tests/e2e/manifests/c-programs.yaml:6456`, verify ci on ptrace) and `privileged_observation_is_refused_deterministically` at `hermit-cli/tests/privileged_observation.rs:30`, which runs that guest with `--backend=ptrace run --strict --verify`.
- T3 (covers F5, F6, F7, F8, F10, F13, F14): with a raw `syscall(SYS_ptrace, ...)`, issue every tracee-only read and write request against a forked guest child and check each returns `EPERM`. Linux would return `ESRCH` there. After the `POKE*`/`SET*` attempts, the child's memory and registers are unchanged: the child prints a known value and exits with a known status.
- T4 (covers F9, F11, F12): `PTRACE_SETOPTIONS` with each `PTRACE_O_*` bit, each resume request, `PTRACE_KILL` and `PTRACE_DETACH` on a guest child all return `EPERM`. The child is neither killed nor signalled: `PTRACE_KILL` must not end it.
- T5 (covers F15): an absent pid, an unknown request number (for example `0x7fff`) and a `NULL` `data` pointer for `PTRACE_GETREGS` all return `EPERM`. This records the deliberate errno difference from Linux's `ESRCH`/`EIO`/`EFAULT`.
- T6 (covers F1, F16): the classic debugger handshake: the child calls `PTRACE_TRACEME` then `raise(SIGSTOP)`, and the parent calls `waitpid`. Under Hermit the child sees `EPERM`, and the parent sees an ordinary `SIGSTOP` stop or exit, never a ptrace stop. The result is identical across the two `--verify` runs.
  Related existing cell: `util-c/pmu-skid` (`tests/e2e/manifests/util-c.yaml:4`), which expects guest exit 3 after the refused `PTRACE_TRACEME`.
- T7 (covers F1, F2): record and replay a guest that calls `PTRACE_TRACEME` and `PTRACE_ATTACH`. Both runs return `EPERM`, and replay needs no recorded host result for them. Today all ptrace C cells have `replay` turned off in the manifest.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/385: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_observation_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2188 returns a fixed EPERM for every request without entering the host, matching the kernel's own answer for an already-traced guest); linux_determinism stays nondeterministic; 16 flag behaviours and 7 tests listed; existing ptrace cells named as candidates but no coverage token recorded because nothing was run against this list, so tracking_status stays todo; nested-tracer emulation left as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed EPERM before any argument check, already realized at detcore/src/lib.rs:2188-2191; kernel check order recorded); target_policy stays refuse; no new F/T ids; removed both open questions (tracer emulation, ESRCH for non-attach requests) and the empty heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ptrace`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ptrace&body=Discussion+for+the+%60ptrace%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fptrace.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ptrace%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: ptrace" (https://github.com/rrnewton/hermit/issues/385), legacy variant A1.

The legacy body's statement that `ptrace` goes through the "UNCLASSIFIED temporary-passthrough block" in `detcore/src/syscall_classification.rs` is stale: since the change the code tags `TODO-HUMAN-REVIEW(PR-853)`, `ptrace` is Determinized and refused (`detcore/src/syscall_classification.rs:562`, `detcore/src/lib.rs:2188`). The unit test `privileged_observation_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2608`) pins the predicate's membership to exactly `ptrace` and `kcmp`.

`hermit-cli/tests/cli.rs` names `SYS_ptrace` in `run_reports_denied_ptrace_and_seccomp_capabilities` (`hermit-cli/tests/cli.rs:5436`). That test denies `ptrace` to Hermit itself and checks Hermit's refusal message. It does not test a guest's ptrace call.

`tests/e2e/manifests/inventory/test-files.json:1369` describes `tests/c/ptrace_eperm.c` as "ci=false", but its cell at `tests/e2e/manifests/c-programs.yaml:6456` has verify `ci: ptrace: true`. The inventory text looks stale; the manifest is what runs.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=ptrace` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `ptrace` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
ptrace(PTRACE_TRACEME)          = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
