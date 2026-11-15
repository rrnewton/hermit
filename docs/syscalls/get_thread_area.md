---
schema: syscall-pages/v1
syscall: get_thread_area
nr_x86_64: 211
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/372
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+get_thread_area&body=Discussion+for+the+%60get_thread_area%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fget_thread_area.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60get_thread_area%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`get_thread_area(u_info)` reads back one of the calling thread's thread-local-storage (TLS) segment descriptors, an interface of the 32-bit x86 ABI. On native x86_64, syscall 211 is not implemented and always returns `ENOSYS`. Hermit forwards it to the host kernel unchanged.

## Linux semantics

The 64-bit syscall table maps number 211 to `sys_ni_syscall`, which returns `-ENOSYS` without reading or writing any argument. The host's generated table (`arch/x86/include/generated/asm/syscalls_64.h` for Linux 7.1.3) has `__SYSCALL(211, sys_ni_syscall)`. The real `sys_get_thread_area` is reachable only through the 32-bit table, as i386 number 244 (`__SYSCALL(244, sys_get_thread_area)` in `syscalls_32.h`). There it takes `entry_number` from the caller's `struct user_desc` and fills in the rest from the thread's TLS slot. That is a different syscall number; on the 32-bit table, number 211 is `getresgid`. For the x86_64 Sysno tracked here, the result is `-ENOSYS` whatever the argument holds, and the caller's buffer is left unchanged.

### Flags and options

- F1: any `u_info` that points at a `struct user_desc`, whatever its `entry_number` (a valid TLS slot index, an out-of-range index or -1): `ENOSYS`. The structure is neither read nor written.
- F2: `u_info` is NULL or points outside the address space: still `ENOSYS`, not `EFAULT`, because the argument is never dereferenced

## Determinism analysis

On native x86_64 the call has no inputs that matter: `sys_ni_syscall` ignores its arguments and touches no state, so every invocation returns `-ENOSYS` and writes nothing to guest memory. The call repeats on every x86_64 host kernel whose 64-bit table leaves 211 unimplemented, which is every kernel the evidence covers (6.17 and 7.1). On the 32-bit ABI the call would return the thread's descriptor, a function of earlier `set_thread_area` calls, but that is i386 number 244, not this Sysno.

### Nondeterminism sources

- None for native x86_64 callers: the entry is an unconditional `ENOSYS`.
- Outside this Sysno: a 32-bit-ABI entry (`int $0x80` with i386 number 244) would read the thread's TLS descriptors. See the open question about how the ptrace backend numbers such entries.

## Hermit design

Target policy: `passthrough`. The code already does this. `get_thread_area` is classified `PassThrough` at `detcore/src/syscall_classification.rs:813`, in the issue-reviewed pass-through block whose comment at `detcore/src/syscall_classification.rs:806` says every matching classification issue recommends PASS-THRU. The generic arm at `detcore/src/lib.rs:2843` forwards it, and the host kernel returns `ENOSYS`. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`; `get_thread_area` at line 2102) pins the classification, not guest behaviour.

Why passthrough:

- Forwarding gives the guest exactly what a native x86_64 process gets, a fixed `ENOSYS` with no memory write. There is nothing to model.
- 64-bit TLS goes through `arch_prctl`, which Detcore determinizes (`detcore/src/syscall_classification.rs:59`).
- Alternative not taken: the fixed-`ENOSYS` refusal set `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:855`, applied at `detcore/src/lib.rs:2091`) covers syscalls "the pinned modern x86_64 kernel leaves unimplemented", which describes `get_thread_area`, but it is not in that set. Moving it would make the `ENOSYS` independent of the host kernel without changing today's guest-visible result. That is a code change and is left as an open question.

Code facts show `subscribed_record_replay: false`: Detcore's `--passthru-opt` subscription leaves it out, so in that mode the call never reaches Detcore.

No test calls `SYS_get_thread_area` (`tests_naming` is 0).

## Recommended tests

- T1 (covers F1): under `hermit run --verify` on ptrace, fill a `struct user_desc` with a sentinel byte pattern and `entry_number` set to 12 (`GDT_ENTRY_TLS_MIN` on an x86_64 kernel), then to 0 and to -1. Call raw `syscall(SYS_get_thread_area, &desc)` each time. Assert `-1`/`ENOSYS` and that every byte of `desc` still holds the sentinel.
- T2 (covers F2): call it with `NULL` and with `(void *)1` and assert `ENOSYS`, not `EFAULT`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/372: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (x86_64 nr 211 is sys_ni_syscall; forwarded at detcore/src/lib.rs:2843); 2 flags, 2 tests; open questions on the ENOSYS refusal set and int 0x80 numbering; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `get_thread_area`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+get_thread_area&body=Discussion+for+the+%60get_thread_area%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fget_thread_area.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60get_thread_area%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: get_thread_area" (https://github.com/rrnewton/hermit/issues/372), legacy variant A2. The legacy body ranked it 93rd in the batch-2 classification sequence, after excluding the 50 workload-observed batch-1 syscalls. Manual reference: Linux man-pages get_thread_area(2).

### Open questions

- Should `get_thread_area` (and `set_thread_area`) join `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:855`), so that the `ENOSYS` is Detcore's and not the host kernel's? Today's guest-visible result is the same. This body keeps `passthrough` until the owner or the classification reviewer decides.
- Detcore classifies by x86_64 Sysno. A 64-bit guest can execute `int $0x80` and reach the 32-bit table, where 244 is the real `get_thread_area` and 211 is `getresgid`. The ptrace backend lives in Reverie, which is not in the exported tree, so this body cannot say whether such an entry is reported under its i386 number with an ABI marker or misread as the x86_64 syscall with the same number.

### 2026-10-03 host probe

Run by syscall-tracking-p3 on the measurement host (Linux 7.1.3): raw `syscall(211, NULL)` returned `ENOSYS`. The host's generated tables show `__SYSCALL(211, sys_ni_syscall)` in `syscalls_64.h`, and `__SYSCALL(244, sys_get_thread_area)` and `__SYSCALL(211, sys_getresgid)` in `syscalls_32.h`. This was the host, not Hermit.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=get_thread_area -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(211, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
get_thread_area(0xffffffffffffffff) = -1 ENOSYS (Function not implemented)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
