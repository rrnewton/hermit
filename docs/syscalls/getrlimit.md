---
schema: syscall-pages/v1
syscall: getrlimit
nr_x86_64: 97
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/378
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrlimit&body=Discussion+for+the+%60getrlimit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrlimit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrlimit%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`getrlimit(resource, rlim)` returns the soft and hard limit of one resource for the calling process. Detcore answers from a fixed per-process table of virtual limits and never asks the host (`handle_getrlimit`, `detcore/src/syscalls/sysinfo.rs:163`). The table starts from the same defaults in every run and changes only through the guest's own `setrlimit` or `prlimit64` calls.

Manual reference: Linux man-pages getrlimit(2).

## Linux semantics

The kernel first rejects `resource >= RLIM_NLIMITS` (16) with `EINVAL`. `resource` is unsigned in the kernel, so a negative value is also out of range. It then reads the live limit pair from the process's shared signal structure and copies it to `rlim`; a bad pointer gives `EFAULT`. On x86_64 the syscall returns `RLIM_INFINITY` as all ones; no old-ABI clamping applies.

Limits are per-process state. Threads share them, `fork` copies them, and `execve` keeps them. The process itself can change them with `setrlimit` or `prlimit64`, and so can another process with `prlimit64` and the right permissions. Modern glibc implements `getrlimit(3)` with `prlimit64`, so only a raw `syscall(SYS_getrlimit, ...)` reaches this syscall.

### Flags and options

- F1: `RLIMIT_CPU` (0): CPU-time limit in seconds
- F2: `RLIMIT_FSIZE` (1): largest file size the process may create
- F3: `RLIMIT_DATA` (2): data segment size
- F4: `RLIMIT_STACK` (3): main-thread stack size; commonly 8 MiB soft on hosts
- F5: `RLIMIT_CORE` (4): core-dump size; often 0 soft on hosts
- F6: `RLIMIT_RSS` (5): resident set size; not enforced by modern Linux
- F7: `RLIMIT_NPROC` (6): processes per real user ID; hosts usually derive the default from memory size
- F8: `RLIMIT_NOFILE` (7): one more than the highest descriptor number the process may open
- F9: `RLIMIT_MEMLOCK` (8): bytes of locked memory
- F10: `RLIMIT_AS` (9): address-space size
- F11: `RLIMIT_LOCKS` (10): file locks; not enforced by modern Linux
- F12: `RLIMIT_SIGPENDING` (11): queued signals per real user ID
- F13: `RLIMIT_MSGQUEUE` (12): bytes in POSIX message queues per real user ID
- F14: `RLIMIT_NICE` (13): ceiling for raising the nice value
- F15: `RLIMIT_RTPRIO` (14): ceiling for real-time priority
- F16: `RLIMIT_RTTIME` (15): real-time CPU time in microseconds before a blocking call
- F17: `resource` out of range (16 or more, or negative): `EINVAL`. Linux checks this before it looks at `rlim`.
- F18: `rlim` NULL or not writable: `EFAULT`
- F19: limit lifetime: shared by every thread of the process, copied by `fork`, kept across `execve`, and changed by `setrlimit`/`prlimit64`, so a later `getrlimit` sees the change
- F20: the reported pair is the limit in force: a later call that needs more than the soft limit `getrlimit` returned fails at exactly that limit, for example `EMFILE` when the next descriptor would equal `RLIMIT_NOFILE`
- F21: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). A failure the reported limit would not predict, such as `EMFILE` from `openat` far below the reported 1048576, is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>`, not handed to the guest

## Determinism analysis

`linux_determinism: conditional` (changed from the rendered `nondeterministic`). On Linux, `getrlimit` is a pure read of process state: started with the same inherited limits, and with no outside process calling `prlimit64` on it, a program reads the same values in every run. The values change between hosts, shells, containers and CI workers, and those differences then change allocation and failure behaviour. That makes the input environmental, not the call itself nondeterministic.

### Nondeterminism sources

- Host or container limit configuration, inherited from the parent (shell, `ulimit`, systemd, PAM)
- Changes made earlier by the process itself or its ancestors
- Another process changing the limit with `prlimit64`
- Resource-specific kernel defaults, for example `RLIMIT_NPROC` and `RLIMIT_SIGPENDING`, which scale with host memory

## Hermit design

Target policy: `model` (changed from `investigate`; legacy recommended action **NEEDS-HANDLER**). The guest sees a virtual limit table that starts from fixed defaults and is never read from the host. The code at this SHA realizes it:

- `handle_getrlimit` (`detcore/src/syscalls/sysinfo.rs:163`) converts `resource`, reads the calling process's table, and writes the pair to `rlim` (`detcore/src/syscalls/sysinfo.rs:181`). It makes no host call.
- Defaults (`ResourceLimits::default`, `detcore/src/tool_local.rs:278`): every one of the 16 resources is `RLIM64_INFINITY` for both soft and hard, except `RLIMIT_STACK` at 8 MiB soft and unlimited hard, and `RLIMIT_NOFILE` at 1048576 for both. `get` returns nothing for resource 16 or more (`detcore/src/tool_local.rs:299`). These defaults cover F1 to F16 identically in every run, whatever the host's limits.
- F19: threads share the table and a new process gets its own copy (`detcore/src/lib.rs:1568`; field comment at `detcore/src/tool_local.rs:1805`). `setrlimit` and `prlimit64` update it virtually; the doc comment at `detcore/src/syscalls/sysinfo.rs:212` says accepted changes are not enforced on the host. Only `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_CORE` may change, and never by raising the hard limit; exact no-op updates succeed for every resource (`validate_resource_limit_mutation`, `detcore/src/syscalls/sysinfo.rs:131`).
- The handler carries `AUTONOMOUS-BOT-IMPLEMENTED` and `TODO-HUMAN-REVIEW(#663)` tags (`detcore/src/syscalls/sysinfo.rs:161`; https://github.com/rrnewton/hermit/issues/663).

`passthrough` would expose the host's inherited limits, and `refuse` would break programs that size descriptor tables or stacks from these limits, so neither is an alternative.

Target for enforcement (owner decision 5, 2026-10-04: limits become real and deterministic). The pair `getrlimit` reports must be the limit actually in force (F20). Hermit gets there per resource, in one of two ways: Detcore enforces the limit itself, counting the resource (for `RLIMIT_NOFILE`, the guest's descriptor table), or Hermit configures the guest's host rlimit to the reported value at launch and again on each accepted `setrlimit`/`prlimit64`. This body does not choose between them; the choice is per resource. Today neither happens: the reported values are fixed (`detcore/src/tool_local.rs:278`-`292`), the table is read only by the three limit handlers and copied at clone (`detcore/src/lib.rs:1568`), and nothing in Detcore applies it to the host (doc comment, `detcore/src/syscalls/sysinfo.rs:212`-`219`). The host kernel keeps enforcing whatever limits the launching shell had.

Interim, until a resource reaches the target (F21): a failure caused by a host-enforced limit that the reported limit would not predict is a determinism failure, not a guest failure. Detcore reports it as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>` through the infrastructure-error convention instead of passing it through. Today it is passed through silently: for example the error arm of `handle_openat` hands the host's `EMFILE` to the guest unchanged (`detcore/src/syscalls/files.rs:1130`-`1132`). That is the gap.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): reads return fixed deterministic values with soft equal to hard for every resource. A raise of the hard limit is then `EPERM` by Linux's own rule for an unprivileged caller, and programs that read their limits at startup (glibc, bash `ulimit`, the JVM, Go and Python runtimes, daemons that close every descriptor up to `RLIMIT_NOFILE`) keep working. The starting table is `RLIMIT_STACK` 8 MiB soft and hard (today 8 MiB soft, unlimited hard, `detcore/src/tool_local.rs:285`-`288`; an unlimited soft stack would select the legacy mmap layout at `execve`), `RLIMIT_NOFILE` 1048576 soft and hard (unchanged), `RLIMIT_MEMLOCK` 0 soft and hard (today unlimited; `mlock`, `mlock2` and `mlockall` are unsupported and return `EPERM`), and `RLIM_INFINITY` soft and hard for every other resource. A lowering through `setrlimit` or `prlimit64` is forwarded to the host and recorded, so `getrlimit` reads it back. Any host-enforced surprise that remains is the `cause=host-rlimit` infrastructure error (F21).

Remaining differences from Linux, from reading the code and not measured:

- **Order of checks (F17, F18).** The handler rejects a negative `resource` with `EINVAL` (`detcore/src/syscalls/sysinfo.rs:168`), then a NULL `rlim` with `EFAULT` (`detcore/src/syscalls/sysinfo.rs:169`), and only then an out-of-range non-negative `resource` with `EINVAL` (`detcore/src/syscalls/sysinfo.rs:175`). So `getrlimit(16, NULL)` returns `EFAULT` under Hermit and `EINVAL` on Linux. The host strace under Additional information shows Linux returning `EINVAL` for resource `0xffffffff` with a bad pointer.
- **The virtual table is not the enforced one (F20, F21).** The host kernel still enforces the host's real limits. A guest that reads `RLIMIT_NOFILE` as 1048576 can still get `EMFILE` earlier if the host limit is lower, and a guest that lowers its own `RLIMIT_NOFILE` to 16 does not get `EMFILE` at 16.

Target (owner decision 6, 2026-10-04): as an emulated call, `handle_getrlimit` makes Linux's checks in the kernel's order. `getrlimit` in `kernel/sys.c` calls `do_prlimit` first, which returns `EINVAL` for `resource >= RLIM_NLIMITS` (the argument is unsigned, so a negative value is out of range too), and only then copies the pair out, so `EFAULT` comes from a failed write and only for a valid resource (F17, F18). The handler should drop the early `EFAULT` for a NULL `rlim` (`detcore/src/syscalls/sysinfo.rs:169`), check the resource range in one place (today split across `detcore/src/syscalls/sysinfo.rs:168` and `detcore/src/syscalls/sysinfo.rs:175`), and let the write at `detcore/src/syscalls/sysinfo.rs:181` produce `EFAULT`. T2 and T3 pin the order.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16): for every resource 0 to 15, a raw `syscall(SYS_getrlimit, ...)` returns the same pair as `prlimit64(0, resource, NULL, &old)` and as glibc `getrlimit`, and the printed values are identical across `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/resource_determinism.c` (cell `c-programs/resource-determinism`, `tests/e2e/manifests/c-programs.yaml:7549`, verify ci on ptrace). Its `check_limit_queries` (`tests/c/resource_determinism.c:100`) calls `SYS_getrlimit` for all 16 resources but compares only `prlimit64` with glibc; the raw result in `syscall_limit` is never checked, so a planted divergence in `handle_getrlimit` would not be caught there. Unit test `defaults_are_fixed_and_cover_linux_resources` at `detcore/src/tool_local.rs:951` pins the defaults.
- T2 (covers F17): raw `getrlimit` with resource 16 and with -1 returns `EINVAL`, both with a valid `rlim` and with NULL. The resource-16-with-NULL case should fail at this SHA because of the check order.
- T3 (covers F18): raw `getrlimit(RLIMIT_NOFILE, NULL)` and with an unmapped address returns `EFAULT`.
- T4 (covers F8, F19): a raw `setrlimit` change to `RLIMIT_NOFILE` is visible through raw `getrlimit` and through `prlimit64`, in another thread, and in a forked child; the child's own change does not reach the parent; and the value survives `execve`.
  Existing candidates: `tests/c/rlimit_identity.c` (cell `c-programs/rlimit-identity`, `tests/e2e/manifests/c-programs.yaml:14992`, verify ci on ptrace), which uses raw syscalls (`tests/c/rlimit_identity.c:85`) for the round trip and the `prlimit64` agreement; `check_limit_mutations` in `tests/c/resource_determinism.c:155`; and unit test `cloned_process_state_changes_independently` at `detcore/src/tool_local.rs:978`. None of them checks another thread, a forked child or `execve`.
- T5 (covers F8, F20): the guest lowers its own `RLIMIT_NOFILE` soft limit to 16 with raw `setrlimit`, reads 16 back with raw `getrlimit`, then opens descriptors until one fails: `EMFILE` must come exactly when the next descriptor would be 16. By the code this fails at this SHA, because the lowered limit is not enforced.
- T6 (covers F21): launch Hermit under a lowered host limit (`prlimit --nofile=64:64` on the launcher). The guest reads `RLIMIT_NOFILE` with raw `getrlimit` (1048576) and opens descriptors until one fails. Assert that the run reports `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=openat errno=EMFILE` and ends with the infrastructure-error exit convention (exit 122 with `class=infrastructure-error cause=host-rlimit` on stderr, as proposed in the error-handling convention), not with the guest seeing `EMFILE` near descriptor 64. If Hermit instead detects at launch that it cannot configure the reported limit, the same report at launch satisfies the test. Today the guest gets the bare `EMFILE`.
- T7 (covers F4, F8, F9): raw `getrlimit` at guest start reports soft equal to hard for all 16 resources, with `RLIMIT_STACK` 8 MiB, `RLIMIT_NOFILE` 1048576, `RLIMIT_MEMLOCK` 0 and every other resource `RLIM_INFINITY`, and the printed table is identical across `hermit run --verify` on ptrace. By the code this fails at this SHA on `RLIMIT_STACK` (unlimited hard) and `RLIMIT_MEMLOCK` (unlimited) (`detcore/src/tool_local.rs:278`-`292`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/378: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (fixed per-process virtual limit table, never read from the host); linux_determinism nondeterministic -> conditional (a pure read of inherited process state); 19 flag behaviours and 4 tests listed; gaps: EFAULT is checked before an out-of-range resource, and no test compares the raw getrlimit result for all 16 resources; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (reported pair is the limit in force, enforced by Detcore or configured on the host per resource; interim host-rlimit infrastructure error, passed through today at files.rs:1130); target_policy stays model; new F20, F21, T5, T6; no question removed (none was about limits); added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (Target: EINVAL for an out-of-range resource before any pointer check, EFAULT only from the copy-out, per getrlimit/do_prlimit in kernel/sys.c); target_policy stays model; no new F/T ids; removed the check-order question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (soft equal to hard for every resource; starting table STACK 8 MiB, NOFILE 1048576, MEMLOCK 0, others RLIM_INFINITY; lowerings forwarded to the host and recorded; leftover host surprises are cause=host-rlimit, F21); target_policy stays model; new T7; removed the standing-policy proposal from Open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getrlimit`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrlimit&body=Discussion+for+the+%60getrlimit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrlimit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrlimit%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getrlimit" (https://github.com/rrnewton/hermit/issues/378), legacy variant A2. It was ranked 99 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### Open questions

- `tests/c/resource_determinism.c:109` reads `SYS_getrlimit` into `syscall_limit` and never compares it. Adding one `require_limit` call would make that cell cover the raw handler for all 16 resources.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=getrlimit -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(97, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
getrlimit(0xffffffff /* RLIMIT_??? */, 0xffffffffffffffff) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
