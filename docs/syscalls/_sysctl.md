---
schema: syscall-pages/v1
syscall: _sysctl
nr_x86_64: 156
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/330
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+_sysctl&body=Discussion+for+the+%60_sysctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2F_sysctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60_sysctl%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unimplemented_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`_sysctl(args)` is the removed binary sysctl interface that read and wrote kernel parameters through integer name vectors. Current x86_64 kernels return `ENOSYS`. Detcore returns that same fixed `ENOSYS` itself and never forwards the call (`detcore/src/lib.rs:2090-2094`).

Manual reference: Linux man-pages _sysctl(2).

## Linux semantics

The argument is one pointer to `struct __sysctl_args { int *name; int nlen; void *oldval; size_t *oldlenp; void *newval; size_t newlen; }` (`/usr/include/linux/sysctl.h:35-43` on the evidence host). Historically a request with `oldval`/`oldlenp` read a parameter, a request with `newval`/`newlen` wrote one, and `nlen` had to be within the name-depth limit. The binary interface was optional (`CONFIG_SYSCTL_SYSCALL`) and was removed in Linux 5.5; glibc removed its `sysctl()` wrapper in 2.32. `/proc/sys` is the replacement.

On current kernels number 156 reaches `sys_ni_syscall` (`__SYSCALL(156, sys_ni_syscall)` in the generated `syscalls_64.h` of both kernel trees under `/usr/src/kernels` on the evidence host), which returns `-ENOSYS` without reading `args`.

### Flags and options

- F1: `args` is `NULL` or points to unmapped memory: `ENOSYS`, not `EFAULT`.
- F2: read request (`oldval` and `oldlenp` set, `newval` `NULL`), for example the name vector `{CTL_KERN, KERN_OSTYPE}`: `ENOSYS`; `*oldlenp` and the `oldval` buffer are left unchanged.
- F3: write request (`newval` and `newlen` set): `ENOSYS`, not `EPERM`; no kernel parameter changes.
- F4: malformed name vector (`nlen` of 0, negative, or above the old depth limit, or an unknown name): `ENOSYS`, not `ENOTDIR` or `EINVAL`.

## Determinism analysis

`linux_determinism: deterministic`. On supported x86_64 kernels the call is an unconditional `ENOSYS` that reads no argument and no kernel state, so it repeats exactly.

### Nondeterminism sources

- None on kernels from 5.5 on.
- On older kernels built with `CONFIG_SYSCTL_SYSCALL`, a read returned live kernel parameters (host name, kernel version, tunables) and a write changed host state. Hermit's fixed `ENOSYS` removes that dependence on the host kernel version.

## Hermit design

Target policy: `refuse`. The rendered body carried `passthrough` from the legacy PASS-THRU recommendation, which predates the code. At hermit f4055eb9cae4 Detcore never forwards `_sysctl` to the host:

- `_sysctl` is listed in the Determinized arm of `classify_syscall` (`detcore/src/syscall_classification.rs:275-287`; the arm ends `=> SyscallClassification::Determinized` at `detcore/src/syscall_classification.rs:688`) and in the predicate `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`).
- The dispatch arm at `detcore/src/lib.rs:2090-2094` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It reads no argument and consults no configuration, so the answer is the same with or without `panic_on_unsupported_syscalls` and under the fail-closed policy.
- The predicate is part of the refusal boundary `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319-1321`), which is why `code_facts.refused` is `true`.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) pins the classification and the exact predicate set. It does not start Hermit, so it is not ptrace coverage.

Why `refuse` and not the alternatives:

- A fixed `ENOSYS` is exactly what every supported x86_64 kernel returns for this number, so a guest sees no difference from native execution. The doc comment at `detcore/src/syscall_classification.rs:851-855` states the same intent: deterministic by construction, without depending on the host kernel being modern.
- `passthrough` contradicts the code and would bring back a host-kernel dependence: on a kernel older than 5.5 built with `CONFIG_SYSCTL_SYSCALL`, a read would return live host kernel parameters and a write would change host state.
- `model` or `emulate` would mean reimplementing an interface that current Linux does not provide, which no program running on a modern kernel can rely on.

The code already realizes `refuse`. What is missing is a test that starts Hermit on ptrace and calls `_sysctl` (none names it at this SHA). The classification, predicate and dispatch arm all still carry `TODO-HUMAN-REVIEW(#715)` (`detcore/src/syscall_classification.rs:271`, `detcore/src/syscall_classification.rs:850`, `detcore/src/lib.rs:2084`; https://github.com/rrnewton/hermit/issues/715). That review concerns the arm as written and does not argue for a different policy.

## Recommended tests

- T1 (covers F1, F2, F3, F4): under `hermit run --verify` on ptrace, call `syscall(SYS__sysctl, ...)` with `NULL`, with `(void *)1`, with a read request for `{CTL_KERN, KERN_OSTYPE}` whose `oldval` buffer and `*oldlenp` are pre-filled with a sentinel, with a write request, and with `nlen` of 0 and of 100. Every call must return `-1` with `errno == ENOSYS`, and the sentinels must be unchanged.
  Shape: a C program `tests/c/sysctl_enosys.c` modelled on `tests/c/futex_wake_enosys.c`, which calls a raw `syscall()` and fails unless it gets `-1`/`ENOSYS`, with a cell modelled on `c-programs/futex-wake-enosys` (`tests/e2e/manifests/c-programs.yaml:1660`, `verify` with `ci: ptrace: true`). Neither the program nor the cell exists for `_sysctl` at this SHA.
- T2 (covers none): record and replay a guest that calls `_sysctl` once; both the recording and the replay must see `-1/ENOSYS`, and the replay must not diverge.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/330#issuecomment-5872928473
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/330: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; _sysctl records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/330 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090-2094, the value current x86_64 kernels return); linux_determinism stays deterministic; 4 flag behaviours, 2 tests; no ptrace test calls _sysctl, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `_sysctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+_sysctl&body=Discussion+for+the+%60_sysctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2F_sysctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60_sysctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: _sysctl" (https://github.com/rrnewton/hermit/issues/330), legacy variant A2. It was ranked 51 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/330.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=_sysctl -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(156, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
_sysctl(0xffffffffffffffff)     = -1 ENOSYS (Function not implemented)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/330#issuecomment-5872928473
