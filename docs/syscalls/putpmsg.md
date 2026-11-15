---
schema: syscall-pages/v1
syscall: putpmsg
nr_x86_64: 182
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/386
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+putpmsg&body=Discussion+for+the+%60putpmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fputpmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60putpmsg%60%2C+or+in+what+the+page+says%3F%0A
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

`putpmsg(fd, ctlptr, dataptr, band, flags)` is the reserved entry for sending a System V STREAMS message. Mainline Linux never implemented STREAMS and returns `ENOSYS`. Detcore returns that same fixed `ENOSYS` itself and never forwards the call (`detcore/src/lib.rs:2090-2094`).

Manual reference: Linux man-pages putpmsg(2) (an alias page of unimplemented(2)).

## Linux semantics

The number is reserved so that out-of-tree STREAMS implementations could use it. In STREAMS, `putpmsg` writes a control part from `ctlptr` and a data part from `dataptr` to a stream, with `flags` selecting `MSG_HIPRI` or `MSG_BAND` and `band` giving the priority band. Mainline Linux maps number 182 to `sys_ni_syscall` (`__SYSCALL(182, sys_ni_syscall)` in the generated `syscalls_64.h` of both kernel trees under `/usr/src/kernels` on the evidence host), which returns `-ENOSYS` without looking at `fd` or any pointer.

### Flags and options

- F1: `flags` of `MSG_HIPRI`, or `MSG_BAND` with any `band`, or 0: `ENOSYS`.
- F2: `fd` is a valid writable descriptor, such as a pipe: `ENOSYS`, and nothing is written to it.
- F3: `fd` is closed or invalid, or a pointer argument is `NULL` or unmapped: `ENOSYS`, not `EBADF` or `EFAULT`.

## Determinism analysis

`linux_determinism: deterministic`. STREAMS was never supported in mainline Linux; the call always returns `ENOSYS` without consulting any argument or state.

### Nondeterminism sources

- None; unconditional `ENOSYS` stub.

## Hermit design

Target policy: `refuse`. The rendered body carried `passthrough` from the legacy PASS-THRU recommendation, which predates the code. At hermit f4055eb9cae4 Detcore never forwards `putpmsg` to the host:

- `putpmsg` is listed in the Determinized arm of `classify_syscall` (`detcore/src/syscall_classification.rs:275-287`; the arm ends `=> SyscallClassification::Determinized` at `detcore/src/syscall_classification.rs:688`) and in the predicate `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`).
- The dispatch arm at `detcore/src/lib.rs:2090-2094` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It reads no argument and consults no configuration, so the answer is the same with or without `panic_on_unsupported_syscalls` and under the fail-closed policy.
- The predicate is part of the refusal boundary `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319-1321`), which is why `code_facts.refused` is `true`.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) pins the classification and the exact predicate set. It does not start Hermit, so it is not ptrace coverage.

Why `refuse` and not the alternatives:

- A fixed `ENOSYS` is exactly what every supported x86_64 kernel returns for this number, so a guest sees no difference from native execution. The doc comment at `detcore/src/syscall_classification.rs:851-855` states the same intent: deterministic by construction, without depending on the host kernel being modern.
- `passthrough` contradicts the code and would bring back a host-kernel dependence: a host kernel patched with an out-of-tree STREAMS implementation would write to real streams and make the result depend on the host.
- `model` or `emulate` would mean reimplementing an interface that current Linux does not provide, which no program running on a modern kernel can rely on.

The code already realizes `refuse`. What is missing is a test that starts Hermit on ptrace and calls `putpmsg` (none names it at this SHA). The classification, predicate and dispatch arm all still carry `TODO-HUMAN-REVIEW(#715)` (`detcore/src/syscall_classification.rs:271`, `detcore/src/syscall_classification.rs:850`, `detcore/src/lib.rs:2084`; https://github.com/rrnewton/hermit/issues/715). That review concerns the arm as written and does not argue for a different policy.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call `syscall(SYS_putpmsg, ...)` on the write end of a pipe with valid `strbuf` structures and `flags` of 0, `MSG_HIPRI` and `MSG_BAND`, then on fd -1 and with `(void *)1` pointers. Every call must return `-1` with `errno == ENOSYS`, and a non-blocking `read()` on the pipe must find no data.
  Shape: a C program `tests/c/putpmsg_enosys.c` modelled on `tests/c/futex_wake_enosys.c`, which calls a raw `syscall()` and fails unless it gets `-1`/`ENOSYS`, with a cell modelled on `c-programs/futex-wake-enosys` (`tests/e2e/manifests/c-programs.yaml:1660`, `verify` with `ci: ptrace: true`). Neither the program nor the cell exists for `putpmsg` at this SHA.
- T2 (covers none): record and replay a guest that calls `putpmsg` once; both the recording and the replay must see `-1/ENOSYS`, and the replay must not diverge.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/386: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090-2094, the value current x86_64 kernels return); linux_determinism stays deterministic; 3 flag behaviours, 2 tests; no ptrace test calls putpmsg, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `putpmsg`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+putpmsg&body=Discussion+for+the+%60putpmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fputpmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60putpmsg%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: putpmsg" (https://github.com/rrnewton/hermit/issues/386), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=putpmsg` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `putpmsg` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

That paragraph describes the code of 2026-07-24. At hermit f4055eb9cae4 Detcore refuses the call with a fixed `ENOSYS` and never forwards it (see Hermit design).

Observed strace line:
```
putpmsg(0xffffffff, 0, 0, 0, 0) = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
