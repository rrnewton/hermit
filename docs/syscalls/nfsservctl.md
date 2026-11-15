---
schema: syscall-pages/v1
syscall: nfsservctl
nr_x86_64: 180
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/312
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+nfsservctl&body=Discussion+for+the+%60nfsservctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnfsservctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60nfsservctl%60%2C+or+in+what+the+page+says%3F%0A
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

`nfsservctl(cmd, argp, resp)` is the removed multiplexed control interface of the in-kernel NFS server. Linux returns `ENOSYS` since 3.1. Detcore returns that same fixed `ENOSYS` itself and never forwards the call (`detcore/src/lib.rs:2090-2094`).

Manual reference: Linux man-pages nfsservctl(2).

## Linux semantics

Before Linux 3.1, `cmd` selected an operation on the kernel NFS server: start a server thread (`NFSCTL_SVC`), add or remove a client, export or unexport a file system, update uid/gid maps, or get a file handle (`NFSCTL_GETFH`). These were commands 0 to 6 in the historical nfsservctl(2). Each read a `struct nfsctl_arg` from `argp`, could write a `union nfsctl_res` to `resp`, and needed administrative privilege. The interface was removed in 3.1 in favour of the nfsd file system. Number 180 now reaches `sys_ni_syscall` (`__SYSCALL(180, sys_ni_syscall)` in the generated `syscalls_64.h` of both kernel trees under `/usr/src/kernels` on the evidence host), which returns `-ENOSYS` without reading any argument.

### Flags and options

- F1: each historical command (0 to 6, `NFSCTL_SVC` to `NFSCTL_GETFH`): `ENOSYS`; `resp` is left unchanged and the host's NFS export table is untouched.
- F2: any other `cmd`, including negative values: `ENOSYS`, not `EINVAL`.
- F3: `argp` or `resp` `NULL` or pointing to unmapped memory: `ENOSYS`, not `EFAULT`.
- F4: a caller that looks privileged (Hermit's virtual uid 0): still `ENOSYS`, not `EPERM` and not success.

## Determinism analysis

`linux_determinism: deterministic`. Since Linux 3.1 the call is a stub that returns `ENOSYS` for every argument, without consulting any state.

### Nondeterminism sources

- None on supported kernels; it is a stub that always returns `ENOSYS`.
- Before 3.1 the result depended on, and could change, the host's NFS server state.

## Hermit design

Target policy: `refuse`. The rendered body carried `passthrough` from the legacy PASS-THRU recommendation, which predates the code. At hermit f4055eb9cae4 Detcore never forwards `nfsservctl` to the host:

- `nfsservctl` is listed in the Determinized arm of `classify_syscall` (`detcore/src/syscall_classification.rs:275-287`; the arm ends `=> SyscallClassification::Determinized` at `detcore/src/syscall_classification.rs:688`) and in the predicate `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`).
- The dispatch arm at `detcore/src/lib.rs:2090-2094` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It reads no argument and consults no configuration, so the answer is the same with or without `panic_on_unsupported_syscalls` and under the fail-closed policy.
- The predicate is part of the refusal boundary `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319-1321`), which is why `code_facts.refused` is `true`.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) pins the classification and the exact predicate set. It does not start Hermit, so it is not ptrace coverage.

Why `refuse` and not the alternatives:

- A fixed `ENOSYS` is exactly what every supported x86_64 kernel returns for this number, so a guest sees no difference from native execution. The doc comment at `detcore/src/syscall_classification.rs:851-855` states the same intent: deterministic by construction, without depending on the host kernel being modern.
- `passthrough` contradicts the code and would bring back a host-kernel dependence: a pre-3.1 host would let the guest read and change the host's NFS server and export state.
- `model` or `emulate` would mean reimplementing an interface that current Linux does not provide, which no program running on a modern kernel can rely on.

The code already realizes `refuse`. What is missing is a test that starts Hermit on ptrace and calls `nfsservctl` (none names it at this SHA). The classification, predicate and dispatch arm all still carry `TODO-HUMAN-REVIEW(#715)` (`detcore/src/syscall_classification.rs:271`, `detcore/src/syscall_classification.rs:850`, `detcore/src/lib.rs:2084`; https://github.com/rrnewton/hermit/issues/715). That review concerns the arm as written and does not argue for a different policy.

## Recommended tests

- T1 (covers F1, F2, F3, F4): under `hermit run --verify` on ptrace, call `syscall(SYS_nfsservctl, cmd, argp, resp)` for every `cmd` from 0 to 6 and for 99 and -1, with zeroed `argp` and a sentinel-filled `resp`; repeat once with `argp` and `resp` `NULL` and once with `(void *)1`. Every call must return `-1` with `errno == ENOSYS`, and `resp` must still hold the sentinel.
  Shape: a C program `tests/c/nfsservctl_enosys.c` modelled on `tests/c/futex_wake_enosys.c`, which calls a raw `syscall()` and fails unless it gets `-1`/`ENOSYS`, with a cell modelled on `c-programs/futex-wake-enosys` (`tests/e2e/manifests/c-programs.yaml:1660`, `verify` with `ci: ptrace: true`). Neither the program nor the cell exists for `nfsservctl` at this SHA.
- T2 (covers none): record and replay a guest that calls `nfsservctl` once; both the recording and the replay must see `-1/ENOSYS`, and the replay must not diverge.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/312#issuecomment-5872888040
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/312: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; nfsservctl records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/312 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090-2094, the value current x86_64 kernels return); linux_determinism stays deterministic; 4 flag behaviours, 2 tests; no ptrace test calls nfsservctl, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `nfsservctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+nfsservctl&body=Discussion+for+the+%60nfsservctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnfsservctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60nfsservctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: nfsservctl" (https://github.com/rrnewton/hermit/issues/312), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/312.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=nfsservctl`, and by confirming hermit currently routes `nfsservctl` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

That paragraph describes the code of 2026-07-24. At hermit f4055eb9cae4 Detcore refuses the call with a fixed `ENOSYS` and never forwards it (see Hermit design).

Observed strace line:
```
nfsservctl(0, 0, 0)             = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/312#issuecomment-5872888040
