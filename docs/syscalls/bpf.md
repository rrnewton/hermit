---
schema: syscall-pages/v1
syscall: bpf
nr_x86_64: 321
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/335
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+bpf&body=Discussion+for+the+%60bpf%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fbpf.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60bpf%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_host_kernel_probe_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`bpf(cmd, attr, size)` is the single multiplexed entry point for eBPF: it creates and queries maps, loads and attaches programs, pins objects in bpffs, loads BTF, and manages links and iterators. Hermit refuses every command with a fixed `ENOSYS` before anything reaches the host (`detcore/src/lib.rs:2329`), which is what a kernel built without `CONFIG_BPF_SYSCALL` returns, so the guest takes its "BPF unavailable" fallback on every host.

Manual reference: Linux man-pages bpf(2).

## Linux semantics

`cmd` selects one of about forty operations, each with its own layout inside `union bpf_attr`. The kernel first checks `size`: a value above one page, or a larger-than-known `attr` whose extra bytes are not zero, fails with `E2BIG` (the host strace under Additional information shows `E2BIG` for `size = 0xffffffff`). The kernel then copies at most `sizeof(union bpf_attr)` bytes, runs the `security_bpf` LSM hook, and dispatches on `cmd`. An unknown `cmd` is `EINVAL`.

Object-creating commands (`BPF_MAP_CREATE`, `BPF_PROG_LOAD`, `BPF_BTF_LOAD`, `BPF_LINK_CREATE`, `BPF_ITER_CREATE`, `BPF_RAW_TRACEPOINT_OPEN`, `BPF_TOKEN_CREATE`, the `*_GET_FD_BY_ID` commands and `BPF_OBJ_GET`) return a new file descriptor, allocated as the lowest free number. Objects are global kernel state: map and program IDs are allocated system-wide, maps can be shared across processes, pinned objects persist in a bpffs mount after the process exits, and attached programs change the behaviour of sockets, cgroups, tracepoints and the network stack for every process they apply to.

Whether a command is permitted depends on the caller's capabilities (`CAP_BPF`, `CAP_PERFMON`, `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`), the `kernel.unprivileged_bpf_disabled` sysctl, BPF tokens delegated from a bpffs mount, and LSM policy. The exact checks differ by command and by kernel version. `BPF_PROG_LOAD` runs the verifier, whose acceptance of a given program, and whose log text, change with kernel version, configuration and CPU features.

### Flags and options

- F1: `size` argument: above one page, or larger than the kernel's `union bpf_attr` with non-zero trailing bytes, gives `E2BIG`. A smaller `size` is zero-extended. An unreadable `attr` gives `EFAULT`.
- F2: unknown `cmd` (for example `0xffffffff`, or a command newer than the running kernel): `EINVAL`.
- F3: `BPF_MAP_CREATE`: creates a map of a given `map_type` (hash, array, per-CPU, ring buffer, LRU, LPM trie, map-in-map, socket and device maps, and others) with `key_size`, `value_size`, `max_entries` and `map_flags` (`BPF_F_NO_PREALLOC`, `BPF_F_RDONLY`, `BPF_F_WRONLY`, `BPF_F_RDONLY_PROG`, `BPF_F_WRONLY_PROG`, `BPF_F_MMAPABLE`, `BPF_F_INNER_MAP`, `BPF_F_NUMA_NODE`, and others); returns a map FD.
- F4: map element commands `BPF_MAP_LOOKUP_ELEM`, `BPF_MAP_UPDATE_ELEM` (with `BPF_ANY`, `BPF_NOEXIST`, `BPF_EXIST`, `BPF_F_LOCK`), `BPF_MAP_DELETE_ELEM`, `BPF_MAP_GET_NEXT_KEY` and `BPF_MAP_LOOKUP_AND_DELETE_ELEM`: read or change one entry of the map named by `map_fd`.
- F5: batch commands `BPF_MAP_LOOKUP_BATCH`, `BPF_MAP_LOOKUP_AND_DELETE_BATCH`, `BPF_MAP_UPDATE_BATCH` and `BPF_MAP_DELETE_BATCH`, and `BPF_MAP_FREEZE`, which makes a map read-only for syscall users.
- F6: `BPF_PROG_LOAD`: verifies and loads a program of a given `prog_type`, with `prog_flags` (`BPF_F_STRICT_ALIGNMENT`, `BPF_F_ANY_ALIGNMENT`, `BPF_F_SLEEPABLE`, `BPF_F_XDP_HAS_FRAGS`, `BPF_F_TEST_RND_HI32`, and others), an optional verifier log buffer and level, BTF and attach-target fields; returns a program FD.
- F7: attachment commands `BPF_PROG_ATTACH`, `BPF_PROG_DETACH` and `BPF_PROG_QUERY` (attach flags `BPF_F_ALLOW_OVERRIDE`, `BPF_F_ALLOW_MULTI`, `BPF_F_REPLACE`, `BPF_F_BEFORE`, `BPF_F_AFTER`, `BPF_F_ID`, `BPF_F_LINK`), and `BPF_RAW_TRACEPOINT_OPEN`.
- F8: link and iterator commands `BPF_LINK_CREATE`, `BPF_LINK_UPDATE`, `BPF_LINK_DETACH`, `BPF_ITER_CREATE` and `BPF_PROG_BIND_MAP`.
- F9: bpffs pinning: `BPF_OBJ_PIN` writes a map, program or link into a bpffs path, and `BPF_OBJ_GET` opens one (with `file_flags` `BPF_F_RDONLY`/`BPF_F_WRONLY`, and `BPF_F_PATH_FD` for a directory-relative path).
- F10: system-wide enumeration and lookup by ID: `BPF_PROG_GET_NEXT_ID`, `BPF_MAP_GET_NEXT_ID`, `BPF_BTF_GET_NEXT_ID`, `BPF_LINK_GET_NEXT_ID`, `BPF_PROG_GET_FD_BY_ID`, `BPF_MAP_GET_FD_BY_ID`, `BPF_BTF_GET_FD_BY_ID` and `BPF_LINK_GET_FD_BY_ID`; these see every BPF object on the host, not only the caller's.
- F11: introspection: `BPF_OBJ_GET_INFO_BY_FD` (IDs, tags, load time, JITed image sizes and addresses), `BPF_TASK_FD_QUERY`, and `BPF_PROG_STREAM_READ_BY_FD` on recent kernels.
- F12: `BPF_BTF_LOAD`: loads BTF type information, with an optional log buffer; returns a BTF FD.
- F13: `BPF_PROG_TEST_RUN` (alias `BPF_PROG_RUN`): runs a loaded program on caller-supplied input, with `repeat` and a reported `duration` in nanoseconds.
- F14: `BPF_ENABLE_STATS` (`BPF_STATS_RUN_TIME`): turns on system-wide run-time accounting until the returned FD is closed.
- F15: `BPF_TOKEN_CREATE`: creates a BPF token from a bpffs mount that delegates selected commands, map types, program types and attach types to an unprivileged user namespace.
- F16: permission outcomes common to every command: `EPERM` or `EACCES` from missing capabilities, `kernel.unprivileged_bpf_disabled`, or an LSM denial through `security_bpf`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. On one host, a fixed sequence of commands from a single process mostly repeats: map contents, element lookups and FD numbers follow from the caller's own actions. That stops holding as soon as any of the following matters, and most real users touch several of them.

### Nondeterminism sources

- Kernel version, configuration and CPU features decide which commands, map types and program types exist, and whether the verifier accepts a program (F2, F3, F6). Verifier logs and JIT output differ between kernels.
- Privilege, the `kernel.unprivileged_bpf_disabled` sysctl and LSM policy decide whether a command succeeds at all (F16). These are host settings that Hermit does not fix.
- IDs are allocated system-wide, and the `*_GET_NEXT_ID` commands enumerate objects that other processes on the host own (F10). `BPF_OBJ_GET_INFO_BY_FD` reports load times and kernel addresses (F11).
- `BPF_PROG_TEST_RUN` reports a wall-clock `duration` (F13), and `BPF_ENABLE_STATS` exposes run-time counters (F14).
- Attached programs and per-CPU maps observe host events, CPU numbers and timing; a map shared with a host process changes under the guest.
- Pinned objects in bpffs persist across runs, so a second run can see what the first one left (F9).

## Hermit design

Target policy: `refuse`. The code already does this at hermit f4055eb9cae4:

- `bpf` is classified `Determinized` (`detcore/src/syscall_classification.rs:552`), with the reason in the comment at `detcore/src/syscall_classification.rs:544`: BPF operations depend on kernel configuration, capabilities and security policy.
- `is_host_kernel_probe_syscall` (`detcore/src/syscall_classification.rs:1277`) matches `bpf`, `cachestat` and `lsm_list_modules`, and the dispatch arm at `detcore/src/lib.rs:2329` returns `Err(Error::Errno(Errno::ENOSYS))` for all three. Nothing is forwarded to the host, no argument is read, and the result does not depend on `--strict` or `--panic-on-unsupported-syscalls`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1334`), which is why `refused: true`. Backends that run guest syscalls outside Detcore's dispatcher use that set to apply the same refusal.
- The unit test `host_kernel_probe_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2514`) pins the membership of the predicate against the whole pinned table.

Because the refusal is decided on the syscall number alone, every flag and command in F1 to F16 collapses to the same answer under Hermit: `-1`/`ENOSYS`, no FD allocated, no object created, `attr` neither read nor written. A kernel without the syscall behaves exactly this way, so a guest that probes for BPF already needs this fallback to run on such kernels.

Why `refuse` and not the alternatives:

- `passthrough` would hand the guest host-global objects, system-wide IDs and kernel-version-dependent verifier verdicts, and an attached program could change host networking or tracing. It would also make the result depend on host privilege and sysctls.
- `model` or `emulate` would mean an eBPF interpreter and verifier inside Detcore, with deterministic map semantics and no access to the hooks programs attach to. A partial model (for example maps only) would still present a kernel that has BPF but rejects most of it, which is a less common shape for callers than a kernel without BPF.
- Refusing with `EPERM` instead of `ENOSYS` was possible, but `EPERM` says the feature exists and the caller lacks permission. `ENOSYS` is the host-independent "unavailable kernel" answer that the comment at `detcore/src/lib.rs:2326` asks for.

What remains is test evidence. The existing tests exercise one shape, `BPF_MAP_CREATE` with a valid array-map `attr` (`tests/c/bpf_enosys.c:27`). The other commands, the bad-`size` case and the no-side-effect property (no FD consumed) are not exercised.

## Recommended tests

- T1 (covers F3): `BPF_MAP_CREATE` with a valid array-map `attr` returns `-1`/`ENOSYS`, under `hermit run --strict --verify` on ptrace.
  Existing candidates: `tests/c/bpf_enosys.c` in cell `c-programs/bpf-enosys` (`tests/e2e/manifests/c-programs.yaml:383`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:402`), and the integration test `host_kernel_probes_fall_back_deterministically` (`hermit-cli/tests/host_kernel_probes.rs:30`), which compiles that guest and runs it with `--backend=ptrace` (`hermit-cli/tests/host_kernel_probes.rs:62`).
- T2 (covers F1, F2, F16): with raw `syscall(SYS_bpf, ...)`, check that `size = 0xffffffff`, `size = 0`, a NULL `attr`, an unknown `cmd` and `cmd = 0xffffffff` all return `ENOSYS`, never `E2BIG`, `EFAULT`, `EINVAL` or `EPERM`, so no host argument check or permission check is visible.
- T3 (covers F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15): issue every command in the build host's `enum bpf_cmd` (`<linux/bpf.h>`) once, each with a plausible `attr` (for element and attach commands use fd `-1`; for `BPF_OBJ_PIN`/`BPF_OBJ_GET` use a path under the guest's `/tmp`), and require `ENOSYS` for each. Fill `attr` with a sentinel and check that no byte changed, so no host value (IDs, `duration`, verifier log) is copied out.
- T4 (covers F3, F6, F12, F15): after a refused object-creating command, `open("/dev/null")` must return the same descriptor number it would have returned before the call, so the refusal allocates no FD.
- T5 (covers F3, F4): record and replay a guest that calls `bpf` and branches on the result; replay must reproduce the `ENOSYS` without a host call. The cell `c-programs/bpf-enosys` has replay disabled (`tests/e2e/manifests/c-programs.yaml:424`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/335#issuecomment-5872939706
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/335: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_host_kernel_probe_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; bpf records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/335 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2329 returns a fixed ENOSYS for every command without reading attr; modelling eBPF is out of scope); linux_determinism stays conditional; 16 flag behaviours and 5 tests listed; existing tests exercise only BPF_MAP_CREATE, no coverage token recorded, so tracking_status stays todo and ptrace_verification untested
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/bpf_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `bpf`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+bpf&body=Discussion+for+the+%60bpf%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fbpf.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60bpf%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: bpf" (https://github.com/rrnewton/hermit/issues/335), legacy variant A2. It was ranked 56 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/335.

The dispatch arm and the predicate carry `TODO-HUMAN-REVIEW(PR-847)` tags (`detcore/src/lib.rs:2326`, `detcore/src/syscall_classification.rs:1271`); the human review they ask for has not been recorded in the code.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=bpf -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(321, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
bpf(0xffffffff /* BPF_??? */, 0xffffffffffffffff, 4294967295) = -1 E2BIG (Argument list too long)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/335#issuecomment-5872939706
