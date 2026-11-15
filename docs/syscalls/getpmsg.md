---
schema: syscall-pages/v1
syscall: getpmsg
nr_x86_64: 181
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/375
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpmsg&body=Discussion+for+the+%60getpmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpmsg%60%2C+or+in+what+the+page+says%3F%0A
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

`getpmsg(fd, ctlptr, dataptr, bandp, flagsp)` is the reserved entry for receiving a System V STREAMS message. Mainline Linux never implemented STREAMS and returns `ENOSYS`. Detcore returns that same fixed `ENOSYS` itself and never forwards the call (`detcore/src/lib.rs:2090-2094`).

Manual reference: Linux man-pages getpmsg(2) (an alias page of unimplemented(2)).

## Linux semantics

The number is reserved so that out-of-tree STREAMS implementations could use it. In STREAMS, `getpmsg` reads a control part into `ctlptr` and a data part into `dataptr` from a stream, with `*flagsp` selecting `MSG_HIPRI`, `MSG_BAND` (with `*bandp`) or `MSG_ANY`. Mainline Linux maps number 181 to `sys_ni_syscall` (`__SYSCALL(181, sys_ni_syscall)` in the generated `syscalls_64.h` of both kernel trees under `/usr/src/kernels` on the evidence host), which returns `-ENOSYS` without looking at `fd` or any pointer.

### Flags and options

- F1: `*flagsp` set to `MSG_HIPRI`, `MSG_BAND` (with `*bandp`) or `MSG_ANY`: `ENOSYS`; `*flagsp`, `*bandp` and both `strbuf` buffers are left unchanged.
- F2: `fd` is a valid readable descriptor with data queued, such as a pipe: `ENOSYS`, and the queued data is not consumed.
- F3: `fd` is closed or invalid, or a pointer argument is `NULL` or unmapped: `ENOSYS`, not `EBADF` or `EFAULT`.

## Determinism analysis

`linux_determinism: deterministic`. Linux does not implement STREAMS `getpmsg` on x86_64; the call returns `ENOSYS` without consulting any argument or state.

### Nondeterminism sources

- None; the current ABI is an unconditional refusal.

## Hermit design

Target policy: `refuse`. The rendered body carried `passthrough` from the legacy PASS-THRU recommendation, which predates the code. At hermit f4055eb9cae4 Detcore never forwards `getpmsg` to the host:

- `getpmsg` is listed in the Determinized arm of `classify_syscall` (`detcore/src/syscall_classification.rs:275-287`; the arm ends `=> SyscallClassification::Determinized` at `detcore/src/syscall_classification.rs:688`) and in the predicate `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`).
- The dispatch arm at `detcore/src/lib.rs:2090-2094` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It reads no argument and consults no configuration, so the answer is the same with or without `panic_on_unsupported_syscalls` and under the fail-closed policy.
- The predicate is part of the refusal boundary `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319-1321`), which is why `code_facts.refused` is `true`.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) pins the classification and the exact predicate set. It does not start Hermit, so it is not ptrace coverage.

Why `refuse` and not the alternatives:

- A fixed `ENOSYS` is exactly what every supported x86_64 kernel returns for this number, so a guest sees no difference from native execution. The doc comment at `detcore/src/syscall_classification.rs:851-855` states the same intent: deterministic by construction, without depending on the host kernel being modern.
- `passthrough` contradicts the code and would bring back a host-kernel dependence: a host kernel patched with an out-of-tree STREAMS implementation would read from real streams and make the result depend on the host.
- `model` or `emulate` would mean reimplementing an interface that current Linux does not provide, which no program running on a modern kernel can rely on.

The code already realizes `refuse`. What is missing is a test that starts Hermit on ptrace and calls `getpmsg` (none names it at this SHA). The classification, predicate and dispatch arm all still carry `TODO-HUMAN-REVIEW(#715)` (`detcore/src/syscall_classification.rs:271`, `detcore/src/syscall_classification.rs:850`, `detcore/src/lib.rs:2084`; https://github.com/rrnewton/hermit/issues/715). That review concerns the arm as written and does not argue for a different policy.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, write a few bytes into a pipe, then call `syscall(SYS_getpmsg, ...)` on the read end with sentinel-filled `strbuf` structures and each of the three flag values, then on fd -1 and with `(void *)1` pointers. Every call must return `-1` with `errno == ENOSYS`, the sentinels must be unchanged, and a following `read()` must still return the bytes written.
  Shape: a C program `tests/c/getpmsg_enosys.c` modelled on `tests/c/futex_wake_enosys.c`, which calls a raw `syscall()` and fails unless it gets `-1`/`ENOSYS`, with a cell modelled on `c-programs/futex-wake-enosys` (`tests/e2e/manifests/c-programs.yaml:1660`, `verify` with `ci: ptrace: true`). Neither the program nor the cell exists for `getpmsg` at this SHA.
- T2 (covers none): record and replay a guest that calls `getpmsg` once; both the recording and the replay must see `-1/ENOSYS`, and the replay must not diverge.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/375: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090-2094, the value current x86_64 kernels return); linux_determinism stays deterministic; 3 flag behaviours, 2 tests; no ptrace test calls getpmsg, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getpmsg`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpmsg&body=Discussion+for+the+%60getpmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpmsg%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getpmsg" (https://github.com/rrnewton/hermit/issues/375), legacy variant A2. It was ranked 96 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=getpmsg -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(181, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
getpmsg(0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff) = -1 ENOSYS (Function not implemented)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
