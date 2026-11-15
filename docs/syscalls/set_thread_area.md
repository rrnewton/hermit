---
schema: syscall-pages/v1
syscall: set_thread_area
nr_x86_64: 205
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/422
  - https://github.com/rrnewton/hermit/issues/440
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_thread_area&body=Discussion+for+the+%60set_thread_area%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_thread_area.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_thread_area%60%2C+or+in+what+the+page+says%3F%0A
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

`set_thread_area(u_info)` installs a thread-local-storage (TLS) segment descriptor in the calling thread's GDT slots, an interface of the 32-bit x86 ABI. On native x86_64, syscall 205 is not implemented and always returns `ENOSYS`; 64-bit TLS uses `arch_prctl(ARCH_SET_FS)`. Hermit forwards it to the host kernel unchanged.

## Linux semantics

The 64-bit syscall table maps number 205 to `sys_ni_syscall`, which returns `-ENOSYS` without reading any argument. The host's generated table (`arch/x86/include/generated/asm/syscalls_64.h` for Linux 7.1.3) has `__SYSCALL(205, sys_ni_syscall)`. The real implementation, `sys_set_thread_area`, is reachable only through the 32-bit table, as i386 number 243 (`__SYSCALL(243, sys_set_thread_area)` in `syscalls_32.h`). There it validates a `struct user_desc`, picks a free TLS slot when `entry_number` is -1 and writes the chosen index back, and loads the descriptor. That entry is a different syscall number; on the 32-bit table, number 205 is `getgroups`. So for the x86_64 Sysno tracked here, the result is `-ENOSYS` whatever the argument holds, and no thread state changes.

### Flags and options

- F1: any `u_info` that points at a `struct user_desc`, including `entry_number = -1` (the 32-bit "allocate a free slot" request) and every combination of its descriptor bits (`seg_32bit`, `contents`, `read_exec_only`, `limit_in_pages`, `seg_not_present`, `useable`, `lm`): `ENOSYS`. The structure is not read, and nothing, including `entry_number`, is written back.
- F2: `u_info` is NULL or points outside the address space: still `ENOSYS`, not `EFAULT`, because the argument is never dereferenced

## Determinism analysis

On native x86_64 the call has no inputs that matter: `sys_ni_syscall` ignores its arguments and touches no state, so every invocation returns `-ENOSYS`. That is why `linux_determinism` is `deterministic`. The legacy `conditional` value came from the 32-bit ABI, where a successful call changes segment-based TLS state; that path is i386 number 243, not the x86_64 Sysno this issue tracks (see the merge note under Additional information). The call repeats on every x86_64 host kernel whose 64-bit table leaves 205 unimplemented, which is every kernel the evidence covers (6.17 and 7.1).

### Nondeterminism sources

- None for native x86_64 callers: the entry is an unconditional `ENOSYS`.
- Outside this Sysno: a 32-bit-ABI entry (`int $0x80` with i386 number 243) would read and change the thread's TLS descriptors. See the open question about how the ptrace backend numbers such entries.

## Hermit design

Target policy: `passthrough`. The code already does this. `set_thread_area` is classified `PassThrough` at `detcore/src/syscall_classification.rs:826`, in the issue-reviewed pass-through block whose comment at `detcore/src/syscall_classification.rs:806` says every matching classification issue recommends PASS-THRU. The generic arm at `detcore/src/lib.rs:2843` forwards it, and the host kernel returns `ENOSYS`. The classification is pinned by the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`; `set_thread_area` at line 2115), which checks the table, not guest behaviour.

Why passthrough:

- Forwarding gives the guest exactly what a native x86_64 process gets, a fixed `ENOSYS`, with no state change. There is nothing to model.
- Hermit's own TLS handling goes through `arch_prctl`, which is Determinized (`detcore/src/syscall_classification.rs:59`). A guest on x86_64 has no working `set_thread_area` to depend on.
- Alternative not taken: Detcore has a fixed-`ENOSYS` refusal set, `is_unimplemented_enosys_syscall` at `detcore/src/syscall_classification.rs:855`, applied at `detcore/src/lib.rs:2091`, for syscalls that "the pinned modern x86_64 kernel leaves unimplemented". `set_thread_area` meets that description but is not in the set. Moving it would make the `ENOSYS` independent of the host kernel without changing what the guest sees today. That is a code change and is left as an open question, not decided here.

Code facts show `subscribed_record_replay: false`: Detcore's `--passthru-opt` subscription leaves it out, so in that mode the call never reaches Detcore.

No test calls `SYS_set_thread_area` (`tests_naming` is 0).

## Recommended tests

- T1 (covers F1): under `hermit run --verify` on ptrace, call raw `syscall(SYS_set_thread_area, &desc)` with `desc.entry_number = -1` and a valid 32-bit data descriptor, then with a second descriptor that sets every flag bit. Assert `-1`/`ENOSYS` each time, and assert that `desc.entry_number` is still -1 afterwards.
- T2 (covers F2): call it with `NULL` and with `(void *)1` and assert `ENOSYS`, not `EFAULT`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/422: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/440 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> deterministic (x86_64 nr 205 is sys_ni_syscall; the stateful path is i386 nr 243); kept passthrough (forwarded at detcore/src/lib.rs:2843); merged duplicate https://github.com/rrnewton/hermit/issues/440; 2 flags, 2 tests; open questions on the ENOSYS refusal set and int 0x80 numbering; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `set_thread_area`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_thread_area&body=Discussion+for+the+%60set_thread_area%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_thread_area.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_thread_area%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: set_thread_area" (https://github.com/rrnewton/hermit/issues/422), legacy variant A1. Manual reference: [set_thread_area(2)](https://man7.org/linux/man-pages/man2/set_thread_area.2.html).

Merged duplicate: https://github.com/rrnewton/hermit/issues/440, filed 2026-07-24 as "Syscall classification: set_thread_area" (legacy variant A1). Comment histories: https://github.com/rrnewton/hermit/issues/422 (this issue) and https://github.com/rrnewton/hermit/issues/440 (0 comments when it was retired).

Merge reconciliation:

- Determinism: this issue said **conditional** ("a compatible 32-bit execution ABI could instead mutate segment-based TLS state"); the duplicate said **DETERMINISTIC** ("on x86-64 it is not implemented ... and returns ENOSYS deterministically"). Deterministic wins for the tracked Sysno, because x86_64 number 205 maps to `sys_ni_syscall` in the host's generated 64-bit table. The 32-bit claim is true but concerns i386 number 243, a different table entry, and is kept above as context and as an open question.
- Both issues recorded `ENOSYS` from the same host class (Linux 6.17 x86_64): this issue with `NULL`, the duplicate with a zeroed `struct user_desc`. Both strace lines are kept below; together they are the evidence for F1 and F2.
- Both recommended PASS-THRU; kept.

### Open questions

- Should `set_thread_area` (and `get_thread_area`) join `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:855`), so that the `ENOSYS` is Detcore's and not the host kernel's? Today's guest-visible result is the same. This body keeps `passthrough` until the owner or the classification reviewer decides.
- Detcore classifies by x86_64 Sysno. A 64-bit guest can still execute `int $0x80` and reach the 32-bit table, where 243 is the real `set_thread_area` and 205 is `getgroups`. The ptrace backend lives in Reverie, which is not in the exported tree, so this body cannot say whether such an entry is reported under its i386 number with an ABI marker or misread as the x86_64 syscall with the same number. Someone with the Reverie source should check that.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=set_thread_area`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `set_thread_area` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
set_thread_area(NULL)           = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### Duplicate https://github.com/rrnewton/hermit/issues/440 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=set_thread_area` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments). Hermit then routed `set_thread_area` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`. The duplicate's kernel note: "x86-64 uses arch_prctl(ARCH_SET_FS) instead. Verified: returns ENOSYS on this x86-64 host."

Observed strace line:
```
set_thread_area({entry_number=0, base_addr=00000000, limit=00000000, seg_32bit=0, contents=0, read_exec_only=0, limit_in_pages=0, seg_not_present=0, useable=0, lm=0}) = -1 ENOSYS (Function not implemented)
```

### 2026-10-03 host probe

Run by syscall-tracking-p3 on the measurement host (Linux 7.1.3): raw `syscall(205, NULL)` returned `ENOSYS`. The host's generated tables show `__SYSCALL(205, sys_ni_syscall)` in `syscalls_64.h`, and `__SYSCALL(243, sys_set_thread_area)` and `__SYSCALL(205, sys_getgroups)` in `syscalls_32.h`. This was the host, not Hermit.
