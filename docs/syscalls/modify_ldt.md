---
schema: syscall-pages/v1
syscall: modify_ldt
nr_x86_64: 154
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/293
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+modify_ldt&body=Discussion+for+the+%60modify_ldt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmodify_ldt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60modify_ldt%60%2C+or+in+what+the+page+says%3F%0A
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

`modify_ldt(func, ptr, bytecount)` reads or writes the process's Local Descriptor Table (LDT), the per-process table of x86 segment descriptors. It is used mainly by Wine and by code that runs 16- or 32-bit segments. Hermit forwards it to the host kernel unchanged. The table belongs to the guest process and holds only what the guest wrote, so forwarding repeats.

## Linux semantics

The LDT belongs to the process's memory map (`mm`): all threads share one table. A `fork` child gets a copy, and `execve` starts with none. A write builds a new table with the changed entry and installs it for every thread of the process. The call multiplexes on `func`. On x86_64 the result is returned as a 32-bit `int`: the kernel casts it to `unsigned int`, so an error is a negative 32-bit value with the upper half of `rax` zero. A raw `syscall()` through glibc therefore reports an error as a large positive number and leaves `errno` alone. Whether the call exists at all, and whether 16-bit segments are accepted, depend on how the host kernel was built (`CONFIG_MODIFY_LDT_SYSCALL`, `CONFIG_X86_16BIT`).

### Flags and options

- F1: `func` 0, read: copies the current table to `ptr`, at most `bytecount` bytes, with `bytecount` capped at 65536 (8192 entries of 8 bytes). It zero-fills the rest up to `bytecount` and returns that byte count. With no table installed it returns 0 and copies nothing.
- F2: `func` 2, read default table: zero-fills `min(bytecount, 128)` bytes at `ptr` and returns that count
- F3: `func` 0x11, write in new mode: `ptr` points to a `struct user_desc` and `bytecount` must equal `sizeof(struct user_desc)` (16). The entry `entry_number` is set from `base_addr`, `limit`, `seg_32bit`, `contents`, `read_exec_only`, `limit_in_pages`, `seg_not_present`, `useable` and, on x86_64, `lm`.
  An all-empty descriptor clears the entry. Returns 0.
- F4: `func` 1, write in old mode: same as F3, except that `useable` (the AVL bit) is ignored and stored as 0. `base_addr == 0 && limit == 0` also clears the entry, and `contents == 3` is always refused.
- F5: write validation: `bytecount` other than 16, `entry_number` of 8192 or more, or `contents == 3` (code segment) with `seg_not_present == 0`: `EINVAL`.
  F10 covers the 16-bit case decided by the kernel build, and F11 a failed allocation.
- F6: `ptr` cannot be read (write) or written (read): `EFAULT`
- F7: any other `func` value: `ENOSYS`.
- F8: scope: threads of one process share the table. A `fork` child starts with a copy of the parent's table, and `execve` discards it.
- F9: return encoding: results, errors included, come back as a 32-bit `int` zero-extended into `rax`. For example `EINVAL` is `0x00000000ffffffea`, and glibc's `syscall()` returns it as 4294967274 without setting `errno`.
- F10: host-config errors: a 16-bit segment (`seg_32bit == 0`) on a kernel built without `CONFIG_X86_16BIT` gives `EINVAL`, and on a kernel built without `CONFIG_MODIFY_LDT_SYSCALL` every call gives `ENOSYS`.
- F11: host-transient error: `ENOMEM` when the kernel cannot allocate the new table.

## Determinism analysis

Every result is a function of `func`, the bytes at `ptr`, `bytecount`, the table the guest itself built earlier, and the host kernel's build options. No other process can change the table, and nothing depends on timing. Under Hermit, threads run one at a time, so a read racing with another thread's write gets a fixed order. The call repeats exactly on any one host kernel.

### Nondeterminism sources

- None at run time. Across hosts, F10 (16-bit segments, and the call compiled out) depends on the kernel build. That is a fixed property of the host, like the kernel version, and does not vary between runs.
- `ENOMEM` (F11) requires the kernel to fail a small allocation, which Hermit does not try to make deterministic.

### Cross-host variation

- Kernel build option `CONFIG_X86_16BIT`: whether a 16-bit segment write succeeds or gives `EINVAL` (F10). The measurement host's 7.1.3 kernel is built without it.
- Kernel build option `CONFIG_MODIFY_LDT_SYSCALL`: whether the call exists at all; without it every call gives `ENOSYS` (F10).

## Hermit design

Target policy: `passthrough`. The code already does this.

- `modify_ldt` is classified `PassThrough` at `detcore/src/syscall_classification.rs:819` (hermit f4055eb9cae4), in the issue-reviewed pass-through block that starts at `detcore/src/syscall_classification.rs:807`. That block's comment (`detcore/src/syscall_classification.rs:808`-`809`) says these calls stay conditional on Hermit's fixed-container and stable-state assumptions.
  The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the class at `detcore/src/syscall_classification.rs:2108`. The census list holds it at `detcore/src/syscall_classification.rs:1726`.
- Interception charges the fixed `_ => FAST_NS` cost (250 ns) at `detcore/src/syscall_time.rs:156`, through `detcore/src/lib.rs:1936`. Code facts show `subscribed_record_replay: false`.

Why passthrough, not `model` or `emulate`: the LDT is hardware state that only the kernel can load. The CPU reads it when the guest loads a segment selector, so Detcore cannot keep a copy of its own. Every input that decides the result comes from the guest.

What this body does not establish: programs call `modify_ldt` so that they can run code in an LDT code segment, and on x86_64 that means 32-bit compatibility mode. System calls made from that mode (`int 0x80`, or `syscall` in a 32-bit segment) use the i386 syscall table. The exported Detcore source has no handling for 32-bit system calls, and the ptrace backend that sees the raw call is in Reverie, which is not in the export. Whether Hermit intercepts compatibility-mode system calls correctly is a separate question (see Open questions). It does not change the policy for `modify_ldt` itself.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, in a fresh process, fill a buffer with `0xAA`. Call raw `syscall(SYS_modify_ldt, 0, buf, 64)` and assert that `(int)` of the result is 0 and the buffer is untouched.
  Call with `func` 2 and `bytecount` 200, and assert 128 with the first 128 bytes zeroed and the rest still `0xAA`.
- T2 (covers F3): write a 32-bit data segment with `func` 0x11 at `entry_number` 2 and assert 0. Read back 64 bytes with `func` 0, assert the return is 64, and check the 8 descriptor bytes at offset 16 (base, limit, type, AVL set from `useable`) and zeroes elsewhere.
  Load the selector `(2 << 3) | 7` into `%fs` or `%gs` and read through it, then restore the selector.
- T3 (covers F4): write the same entry with `func` 1 and `useable = 1` and assert that the read-back AVL bit is 0. Write `base_addr = 0, limit = 0` with `func` 1 and assert that the entry reads back as zero.
- T4 (covers F5, F7): assert `EINVAL` (compared as `(int)ret`) for `bytecount` 15, for `entry_number` 8192, for `contents = 3` with `seg_not_present = 0`, and for `contents = 3` in old mode. Assert `ENOSYS` for `func` 3.
  For `seg_32bit = 0`, record whether the host returns 0 or `EINVAL` (F10, covered by U2). The test must state the host's `CONFIG_X86_16BIT` rather than assume it.
- T5 (covers F6): assert `EFAULT` for a write with `ptr = (void *)1`, and for a read with `ptr = (void *)1` and `bytecount = 8` after an entry has been written
- T6 (covers F8): after writing an entry, a second thread reads it back. A `fork` child reads the same entry and then writes a different one, and the parent still reads its original. After `execve`, `func` 0 returns 0.
- T7 (covers F9): assert that a failing raw `syscall(SYS_modify_ldt, 0x11, &desc, 15)` returns `0xffffffea` as a 64-bit value, with `errno` unchanged. This pins that Hermit forwards `rax` exactly as the kernel produced it, without sign-extending it into a 64-bit `-EINVAL`.
- U1 (covers F11; host-transient): `ENOMEM` for the new table depends on host memory shared with other processes. The plain passthrough (`detcore/src/syscall_classification.rs:819`, `detcore/src/lib.rs:2843`) hands it to the guest today. F11 counts toward done only once Hermit hides this error or reports it as an infrastructure error.
- U2 (covers F10; host-config): the CI host's kernel build is fixed, so CI sees only one answer for 16-bit segments and cannot produce the compiled-out `ENOSYS`. Hermit forwards the kernel's result unchanged (`detcore/src/syscall_classification.rs:819`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/293#issuecomment-5872845984
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/293: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; modify_ldt records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/293 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (the LDT is guest-owned, kernel-loaded state; forwarded at detcore/src/lib.rs:2843); listed 9 behaviours (3 func modes, old/new write mode, errors, scope, 32-bit return encoding) and 7 tests; no test names SYS_modify_ldt, so tracking_status stays todo; noted compat-mode syscall interception as an unverified adjacent question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (kernel build options CONFIG_X86_16BIT, CONFIG_MODIFY_LDT_SYSCALL); F5 and F7 narrowed to guest-determined errors; new F10, the kernel-build EINVAL and ENOSYS (host-config U2), and F11, ENOMEM (host-transient U1); no open question resolved

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `modify_ldt`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+modify_ldt&body=Discussion+for+the+%60modify_ldt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmodify_ldt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60modify_ldt%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: modify_ldt" (https://github.com/rrnewton/hermit/issues/293), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/293.

The legacy body named `func` values 0, 1 and 0x11. This body adds `func` 2 (read the default table) and the `ENOSYS` result for other values. It keeps the legacy determinism claim: the LDT is per-process guest state, and a read of an unset table returns 0.

### Open questions

- Not a policy question for this syscall: does Hermit's ptrace backend intercept and dispatch system calls made from 32-bit compatibility mode (`int 0x80`, or `syscall` in a 32-bit LDT code segment) correctly? The i386 numbering differs from x86_64. The exported Detcore source has no handling for it, and the backend is in Reverie, outside the export. Wine-style guests are the main users of `modify_ldt` and would hit this.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=modify_ldt`, and by confirming hermit currently routes `modify_ldt` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
modify_ldt(0, 0x406000, 0)      = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/293#issuecomment-5872845984
