---
schema: syscall-pages/v1
syscall: lookup_dcookie
nr_x86_64: 212
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/635
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lookup_dcookie&body=Discussion+for+the+%60lookup_dcookie%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flookup_dcookie.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lookup_dcookie%60%2C+or+in+what+the+page+says%3F%0A
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

`lookup_dcookie(cookie, buffer, len)` (x86_64 number 212) once returned the path of a directory-entry cookie handed out to a kernel profiler such as oprofile. Current mainline kernels no longer implement it and wire the slot to `sys_ni_syscall`, which returns `ENOSYS`. Detcore answers it itself with a fixed `ENOSYS` and never forwards it to the host kernel (`detcore/src/lib.rs:2090` at hermit f4055eb9cae4).

Manual reference: Linux man-pages lookup_dcookie(2).

## Linux semantics

Older kernels built with `CONFIG_PROFILING` implemented the call in `fs/dcookies.c`. A profiler registered as a dcookie user, the kernel handed it 64-bit cookies naming the dentries of sampled code, and `lookup_dcookie` turned a cookie back into a path. That code has since been removed from mainline. On the 6.13 and 7.1 kernels installed on the evidence host, the generated x86_64 table (`arch/x86/include/generated/asm/syscalls_64.h`) reads `__SYSCALL(212, sys_ni_syscall)`, and the generic table (`include/uapi/asm-generic/unistd.h`) maps `__NR_lookup_dcookie` to `sys_ni_syscall` as well. `sys_ni_syscall` ignores every argument, reads and writes no user memory, and returns `-ENOSYS`. The exact release that removed the code is not pinned here.

The call takes no flags. On kernels that implemented it, its result depended on its arguments, on the caller's privilege and on profiler state, as lookup_dcookie(2) documents. Each of those behaviours is listed below together with what a kernel without dcookies, and Hermit, return instead.

### Flags and options

- F1: `buffer` and `len`: on implementing kernels the path, with its terminating NUL, was copied into `buffer` and its length returned, or `ERANGE` if `len` was too short and `EFAULT` for a bad pointer. Without dcookies, and under Hermit, the call returns `ENOSYS` and `buffer` is not written.
- F2: `cookie`: on implementing kernels, `EINVAL` when no profiler was registered as a dcookie user or the cookie named no live dentry; otherwise the result depended on which cookies the profiler had been given. Without dcookies, and under Hermit, `ENOSYS` for every cookie value.
- F3: privilege: on implementing kernels a caller without `CAP_SYS_ADMIN` got `EPERM` before any other check. Without dcookies, and under Hermit, `ENOSYS` whatever the caller's capabilities.

## Determinism analysis

`linux_determinism: conditional`; the legacy classification said `nondeterministic`. Natively the answer depends on the kernel:

- On current mainline kernels, where slot 212 is `sys_ni_syscall`, the call returns `ENOSYS` for every argument and repeats exactly.
- On older kernels built with `CONFIG_PROFILING`, the result depended on whether a profiler was running and which dentries it had sampled. That is host activity outside the program, so the call did not repeat (F1, F2). The legacy note "Queries a host profiling cookie cache populated by nondeterministic kernel activity" describes this case.

The legacy `nondeterministic` is replaced by `conditional` because the profiler dependency exists only on kernels that still carry the dcookie code, and is absent from current mainline.

### Nondeterminism sources

- Kernel version and configuration: whether the dcookie code exists at all.
- On implementing kernels: whether a profiler had registered, and which dentries it had been handed cookies for.
- Host-dependent only: a host seccomp policy that answers the syscall with another errno.

## Hermit design

Target policy: `refuse`, a fixed `ENOSYS` that Detcore produces without consulting the host. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` puts `lookup_dcookie` in the Determinized class, in the group tagged `TODO-HUMAN-REVIEW(#715)` (https://github.com/rrnewton/hermit/issues/715) for syscalls the kernel leaves at `sys_ni_syscall` (`detcore/src/syscall_classification.rs:270`-`287`).
- `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`) lists the same 13 syscalls, including `lookup_dcookie`.
- In `handle_syscall_event`, the match on `classify_syscall` (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2090`-`2094`, which returns `Err(Error::Errno(Errno::ENOSYS))`. The guest's syscall is never executed on the host, so `buffer` is never written. The arm does not depend on `panic_on_unsupported_syscalls`, so the result is the same with fail-closed on (the `hermit run` default) or off (`--allow-unsupported-syscalls`).
- The syscall is intercepted in both subscriptions: run mode subscribes every syscall (`detcore/src/lib.rs:1094`), and the `passthru_opt` subscription that `hermit record` and `hermit replay` use adds every Determinized syscall (`detcore/src/lib.rs:1300`-`1302`).
- It is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), the set other execution paths must answer with the same fixed errno.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) checks that the classification and the helper agree on exactly these 13 syscalls. It does not launch Hermit, so it says nothing about the guest-visible result.

Why `refuse`, in place of the legacy `investigate` (from "NEEDS-HANDLER"):

- The fixed `ENOSYS` is exactly what current mainline returns, so programs see a kernel without dcookies, which they must already handle.
- On an older host that still implements the call, a refusal hides the host profiler state that made the call nondeterministic, along with host paths the guest should not see.
- `model` or `emulate` would mean inventing a dcookie table for a profiler interface that no longer exists in mainline; no guest workload needs that.
- `passthrough` would make the guest's answer depend on the host kernel, the case the legacy classification called nondeterministic.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, a C program fills a 4096-byte buffer with a known byte pattern, then calls `syscall(212, cookie, buffer, len)` with `(0, buffer, 4096)`, `(UINT64_MAX, buffer, 4096)`, `(1, buffer, 0)` and `(1, NULL, 4096)`. It prints each return value and `errno`, and whether the buffer still holds the pattern. Every call must return `-1`/`ENOSYS`, with the buffer untouched, identical in both runs, and equal to a native run on a kernel where slot 212 is `sys_ni_syscall`.
- T2 (covers F3): run the T1 calls in a guest that keeps Hermit's default virtual root identity and again after the guest drops `CAP_SYS_ADMIN` (for example with `capset` or `setpriv`). Both must return `ENOSYS`, never `EPERM`.
- T3 (covers none): run the T1 program with fail-closed on (the default) and with `--allow-unsupported-syscalls`. In both, every call returns `ENOSYS` and the run continues to a zero exit; fail-closed must not stop the run, because this is a deterministic refusal, not an unsupported syscall.
- T4 (covers F1, F2): `hermit record` the T1 program, then `hermit replay` it. Replay must print the same `ENOSYS` results and the same untouched buffer.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/635: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090 without reaching the host, identical to current mainline, where slot 212 is sys_ni_syscall); linux_determinism nondeterministic -> conditional (profiler-dependent only on older kernels that still carry fs/dcookies.c); 3 argument behaviours and 4 tests listed, none run against this list, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lookup_dcookie`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lookup_dcookie&body=Discussion+for+the+%60lookup_dcookie%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flookup_dcookie.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lookup_dcookie%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lookup_dcookie" (https://github.com/rrnewton/hermit/issues/635), legacy variant B. The legacy summary asked to "replace its temporary catch-all policy with an explicit Detcore classification"; that is done at hermit f4055eb9cae4 (see Hermit design).

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
