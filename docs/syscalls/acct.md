---
schema: syscall-pages/v1
syscall: acct
nr_x86_64: 163
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/331
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+acct&body=Discussion+for+the+%60acct%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Facct.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60acct%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_process_isolation_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`acct(filename)` turns BSD process accounting on (appending one record per exiting process to `filename`) or off (`filename == NULL`) for the caller's PID namespace. Detcore refuses every call with a fixed `EPERM` before the kernel sees it (`detcore/src/lib.rs:2172-2176`), in every mode and without any argument check.

Manual reference: Linux man-pages acct(2).

## Linux semantics

The kernel checks `capable(CAP_SYS_PACCT)` first, against the initial user namespace. A caller without that capability, including root inside a user namespace, gets `EPERM` before `filename` is read, so even a bad pointer gives `EPERM`. On a kernel built without `CONFIG_BSD_PROCESS_ACCT`, the syscall is not implemented and every call returns `ENOSYS`; the host strace under Additional information, `acct(0xffffffffffffffff) = -1 ENOSYS`, comes from such a kernel.

When the caller is privileged, a non-NULL `filename` is opened for appending and installed as the accounting file of the caller's active PID namespace, replacing any previous one. From then on, every process that exits in that namespace or a descendant namespace appends one fixed-size record (command name, user and group IDs, start time, elapsed, user and system time, memory and I/O counts, exit code, flags). Accounting suspends itself when free space on the file's filesystem drops below a threshold and resumes when it rises again (sysctl `kernel.acct`, checked periodically). `acct(NULL)` stops accounting for the namespace.

### Flags and options

- F1: privilege check: without `CAP_SYS_PACCT` in the initial user namespace the call returns `EPERM`. This check runs before the argument is examined, so it wins over `EFAULT` and every path error.
- F2: kernel built without `CONFIG_BSD_PROCESS_ACCT`: every call returns `ENOSYS`, whatever the argument.
- F3: `filename` non-NULL, privileged caller: open `filename` for appending and start accounting to it for the caller's PID namespace, replacing any accounting file already active there. Returns 0.
- F4: `filename` non-NULL, path errors: `EFAULT` (bad pointer), `ENAMETOOLONG`, `ENOENT`, `ENOTDIR`, `ELOOP`, `EACCES` (no write or search permission, or the file is not a regular file), `EISDIR`, `EROFS`, `EIO` (file cannot be written), `ENFILE`/`ENOMEM`/`EUSERS` (resource exhaustion).
- F5: `filename == NULL`, privileged caller: stop accounting for the caller's PID namespace. Returns 0 even if accounting was already off.
- F6: side effect while accounting is on: one record appended per process exit in the namespace and its descendants, with suspension and resumption driven by free space on the accounting filesystem.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. The return value alone depends on host facts the guest does not control: whether the kernel has BSD accounting (`ENOSYS` versus `EPERM`), and whether the caller holds `CAP_SYS_PACCT` in the initial user namespace. Once accounting is on, the file's content is driven by every process exit in the namespace, including processes outside the guest when the namespace is shared, and each record carries wall-clock start times, elapsed and CPU times, PIDs and resource counts. Suspension depends on host free space. The call repeats only for the refusal paths on one fixed host configuration.

### Nondeterminism sources

- Host kernel configuration (`CONFIG_BSD_PROCESS_ACCT`) and host capabilities decide `ENOSYS`, `EPERM` or success
- Host-global (per PID namespace) process lifecycle: which processes exit, and when
- Wall-clock and CPU times, PIDs and memory or I/O counts written into each record
- External filesystem state: the path, its permissions, and free space that suspends accounting

## Hermit design

Target policy: `refuse`, which is what the code does at hermit f4055eb9cae4.

- `acct` is classified `Determinized` (`detcore/src/syscall_classification.rs:527`) and belongs to `is_process_isolation_refused_syscall` (`detcore/src/syscall_classification.rs:1137-1141`). The dispatch arm at `detcore/src/lib.rs:2172-2176` returns `Err(Error::Errno(Errno::EPERM))` without reading the argument or contacting the kernel, and the arm has no mode or fail-closed condition, so `run`, `--verify`, `--strict` and record/replay all see the same `EPERM`.
- The set is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1328`) and not of the fail-closed-only set, which matches `refused: true` and `refused_only_when_fail_closed: false` in the code facts.
- The doc comment at `detcore/src/syscall_classification.rs:1128-1136` gives the reason: `acct` mutates accounting state outside the container and writes to a file chosen at host level, and a fixed `EPERM` does not depend on host capabilities or LSM policy.

Why `refuse`, and why `EPERM`:

- `passthrough` would let the guest change accounting for every process in its PID namespace and produce records full of wall-clock and host data; it also leaks the host kernel configuration through `ENOSYS`.
- `model` or `emulate` would mean a virtual accounting file with deterministic records. No workload is known to need this, and accounting is an administrator facility, not something a program relies on.
- `EPERM` is exactly what Linux returns to an unprivileged caller, or to root inside a user namespace, on a kernel with BSD accounting (F1). Hermit therefore matches the native answer on such hosts and replaces the host-dependent `ENOSYS` (F2) with that same answer everywhere. Under Hermit, F2-F6 are never reached; the tests below check that each of those argument classes still gets `EPERM` and causes no side effect.

The policy is realized in code. What is missing for `done` is test evidence that covers each argument class (only `acct(NULL)` is tested today) and a recorded ptrace result.

## Recommended tests

- T1 (covers F1, F5): `syscall(SYS_acct, NULL)` under `hermit run --strict --verify` on ptrace returns -1 with `errno == EPERM`, identically in both runs.
  Existing candidates: `tests/c/acct_refusal_probe.c` (cell `c-programs/acct-refusal-probe`, `tests/e2e/manifests/c-programs.yaml:124`, verify `ci: ptrace: true`), also run by `process_isolation_refusals_verify` in `hermit-cli/tests/process_isolation_refusals.rs:34`, which passes `--backend=ptrace` (`hermit-cli/tests/process_isolation_refusals.rs:69`).
- T2 (covers F1, F3, F6): `acct` on a writable regular file inside the guest's directory returns `EPERM`; afterwards the guest forks and reaps a child, and the file is still empty (no record was appended).
- T3 (covers F1, F4): `acct((char *)-1)`, a nonexistent path, a directory and a path longer than `PATH_MAX` each return `EPERM`, never `EFAULT`, `ENOENT`, `EISDIR` or `ENAMETOOLONG`.
- T4 (covers F2): the refusal does not depend on the host kernel: the T1 expectation (`EPERM`) holds on a host whose native `acct` returns `ENOSYS`. Record the host's native result next to Hermit's in the test log so the difference is visible.
- T5 (covers F1, F5): record and replay a guest that calls `acct(NULL)` and prints the errno; replay prints the same `EPERM` line.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/331#issuecomment-5872930748
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/331: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_process_isolation_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; acct records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/331 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (investigate -> refuse; Detcore returns a fixed EPERM for every call at detcore/src/lib.rs:2172-2176, the native answer for an unprivileged or user-namespace caller); linux_determinism stays nondeterministic; 6 flag behaviours and 5 tests listed; the existing probe covers only acct(NULL) and no ptrace result was inspected, so tracking_status stays todo and ptrace_verification untested
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/acct_refusal_probe.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `acct`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+acct&body=Discussion+for+the+%60acct%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Facct.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60acct%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: acct" (https://github.com/rrnewton/hermit/issues/331), legacy variant A2. It was ranked 52 in the batch-2 classification sequence, which excluded the 50 workload-observed batch-1 syscalls.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/331.

The refusal was added by the change the code tags `TODO-HUMAN-REVIEW(PR-844)` (`detcore/src/lib.rs:2170`); that tag still asks for a human review of the policy.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=acct -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(163, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
acct(0xffffffffffffffff)        = -1 ENOSYS (Function not implemented)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

The `ENOSYS` result shows that this host kernel was built without BSD process accounting (F2): the privilege check, which would otherwise have returned `EPERM`, was never reached.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/331#issuecomment-5872930748
