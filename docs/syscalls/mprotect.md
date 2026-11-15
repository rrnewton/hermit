---
schema: syscall-pages/v1
syscall: mprotect
nr_x86_64: 10
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mprotect&body=Discussion+for+the+%60mprotect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmprotect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mprotect%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 1
---

## Summary

`mprotect(addr, len, prot)` changes the access protection of the pages in `[addr, addr+len)` of the calling process's address space. Hermit forwards it to the host kernel unchanged: Detcore keeps no model of page protections, and the result depends only on the guest's own mapping layout and on host policy fixed at launch.

## Linux semantics

The kernel rounds `len` up to a page multiple, requires `addr` to be page aligned, and walks every VMA in the range. Each VMA whose protection changes may be split into up to three VMAs, so the call can fail on the per-process map-count limit. Making a private writable mapping out of a read-only one charges commit memory, so it can fail under strict overcommit. The call returns 0 or a negative errno, has no output buffer, and its only effect is the new protection, which the guest observes by touching the memory (a `SIGSEGV` on a forbidden access) or by reading `/proc/self/maps`. Threads share the address space, so a change made by one thread is seen by all of them.

### Flags and options

- F1: `prot` access bits: `PROT_NONE`, `PROT_READ`, `PROT_WRITE` and `PROT_EXEC`, in any combination; on x86_64 `PROT_WRITE` implies readability and `PROT_SEM` is accepted and ignored
- F2: `PROT_GROWSDOWN` / `PROT_GROWSUP`: extend the change to the start or end of a growable (stack) VMA; both together, or either on a VMA that does not grow in that direction, gives `EINVAL`
- F3: unknown bits in `prot`, or an `addr` that is not page aligned: `EINVAL`
- F4: `len` is rounded up to a page multiple; `len == 0` succeeds without touching anything
- F5: part of the range is not mapped, or `addr + len` overflows: `ENOMEM`
- F6: `PROT_WRITE` on a `MAP_SHARED` mapping of a file opened read-only, or `PROT_EXEC` on a mapping from a `noexec` mount: `EACCES`
- F7: the change would split VMAs past `vm.max_map_count`: `ENOMEM`
- F8: the `READ_IMPLIES_EXEC` personality adds `PROT_EXEC` whenever `PROT_READ` is requested
- F9: a range sealed with `mseal`: `EPERM`
- F10: the protection key of each VMA is kept; only `pkey_mprotect` changes it
- F11: a Linux security module (SELinux execmem/execmod, for example) refuses the change: `EACCES`
- F12: under strict overcommit (`vm.overcommit_memory=2`), making a private read-only mapping writable fails its commit charge once the system-wide commit limit, shared with every other host process, is reached: `ENOMEM`

## Determinism analysis

The result is a function of the guest's address-space layout, the requested range and protection, the process personality, and host policy: mount flags, `vm.max_map_count`, the overcommit mode and any security module. The layout itself comes from the guest's own `mmap`, `brk` and exec history. Hermit runs the guest with `ADDR_NO_RANDOMIZE` (stated at `tests/c/personality_domain.c:12`) and serializes guest threads, so the same program places its mappings at the same addresses on the same host, and the same `mprotect` sees the same VMAs. The call repeats under those conditions, which is why `linux_determinism` is conditional rather than deterministic.

### Nondeterminism sources

- Mapping placement that differs between hosts or kernels (a different `mmap_min_addr`, a different stack limit or a different kernel placement policy changes which ranges are mapped, so F5 and F7 can change).
- Host policy behind F6, F7 and F11: mount flags, `vm.max_map_count`, the overcommit mode and LSM configuration.
- Commit-charge failures that depend on host-wide memory pressure under strict overcommit (F12).

### Cross-host variation

- Sysctl `vm.max_map_count`: how many VMA splits succeed before `ENOMEM` (F7).
- Overcommit sysctl `vm.overcommit_memory`: only strict mode (2) makes the commit-charge failure of F12 possible.
- Linux security module and its policy (SELinux execmem/execmod, for example): whether an executable or writable-executable change gives `EACCES` (F11).

## Hermit design

Target policy: `passthrough`. The code already does this. `mprotect` is classified `PassThrough` at `detcore/src/syscall_classification.rs:706`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`; the comment at `detcore/src/syscall_classification.rs:691` states the condition ("conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions"). It is also listed in the `PASSTHROUGH_SYSCALLS` census at `detcore/src/syscall_classification.rs:1727`. The blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it, and `passthrough()` at `detcore/src/lib.rs:367` hands it to the record-or-replay helper at `detcore/src/tool_local.rs:349`. Its virtual cost is the memory class at `detcore/src/syscall_time.rs:93`.

Why passthrough, not `model` or `emulate`:

- Hermit has no model of page protections. The neighbouring memory calls that do have typed handlers (`mmap`, `madvise`, `mincore`, `munmap`, `mremap` at `detcore/src/lib.rs:2403`) handle placement and content, and `MemoryMetadata` (`detcore/src/memory.rs:54`) records no protection bits. Only the kernel can enforce a protection, so emulating it would mean reimplementing the MMU.
- Every input the call reads is either guest-owned state that Hermit already keeps repeatable (the layout, under `ADDR_NO_RANDOMIZE` and serialized scheduling) or host policy that is fixed for the life of a run.
- The sibling `pkey_mprotect` is also pass-through, at `detcore/src/syscall_classification.rs:823`.

Code facts show `subscribed_record_replay: false`: `mprotect` is not in the `--passthru-opt` list that starts near `detcore/src/lib.rs:1214`, so in that mode it never reaches Detcore. `hermit record` uses its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree; this body does not describe how record treats `mprotect`.

The open gap is test evidence. `tests_naming` is 0: no test calls `SYS_mprotect` directly; existing cells use the glibc wrapper.

## Recommended tests

- T1 (covers F1, F4, F10): under `hermit run --verify` on ptrace, map anonymous pages and call raw `syscall(SYS_mprotect, ...)` with each of `PROT_NONE`, `PROT_READ`, `PROT_READ|PROT_WRITE`, `PROT_READ|PROT_EXEC` and `PROT_READ|PROT_SEM`, plus `len == 0` and an unaligned `len`. After each call print the matching `/proc/self/maps` line (permissions only, not addresses) and, for `PROT_NONE` and `PROT_READ`, prove a forbidden write raises `SIGSEGV` via a handler that `siglongjmp`s out. Check that a key set with `pkey_mprotect` survives a later `mprotect`, if the host has protection keys; otherwise the test must fail, not skip.
  Existing partial coverage via glibc `mprotect()`: `tests/c/signal_delivery_sequence.c:136` and `tests/c/signal_delivery_sequence.c:152` (PROT_NONE guard pages and PROT_READ|PROT_WRITE restore; cell `c-programs/signal-delivery-sequence`, `tests/e2e/manifests/c-programs.yaml:17172`, ptrace verify in CI), and `tests/c/mmap_stress_determinism.c:48` and `tests/c/mmap_stress_determinism.c:53` (PROT_READ then PROT_READ|PROT_WRITE across a multi-page mapping; cell `c-programs/mmap-stress-determinism`, `tests/e2e/manifests/c-programs.yaml:3834`, ptrace verify in CI).
- T2 (covers F2): assert that `PROT_GROWSDOWN` on the main thread's stack VMA succeeds, and that `PROT_GROWSDOWN|PROT_GROWSUP` or `PROT_GROWSDOWN` on an ordinary anonymous mapping gives `EINVAL`.
- T3 (covers F3, F5): assert `EINVAL` for an unaligned `addr` and for `prot = 0x80000`, and `ENOMEM` for a range with an unmapped hole and for `addr + len` wrapping.
  Existing partial coverage via glibc `mprotect()`: `tests/c/errno_path_identity.c:118` (`mprotect((void*)0x1, 1, PROT_READ)` reports `EINVAL`; cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`).
- T4 (covers F6): map a file opened `O_RDONLY` with `MAP_SHARED|PROT_READ` and assert `PROT_WRITE` gives `EACCES`; where the guest can see a `noexec` mount, assert `PROT_EXEC` on a mapping from it gives `EACCES`, and record the mount used.
- T5 (covers none): alternate protections page by page across a large mapping until the split fails, and assert `ENOMEM` at a count that matches `/proc/sys/vm/max_map_count` read at the start. The strict-overcommit failure is F12, recorded by U1, and this test does not cover it. On a host whose `vm.max_map_count` is the Linux default of 65530 this reaches the F7 `ENOMEM`; F7 itself is host configuration, covered by U3.
- T6 (covers F8): call `personality(READ_IMPLIES_EXEC)`, `mprotect` a page `PROT_READ`, and assert the `/proc/self/maps` line shows `x`.
- T7 (covers F9): where the host kernel provides `mseal`, seal a page and assert `mprotect` gives `EPERM`. On an older host kernel the test must report the missing syscall as a failure, not a skip.
- T8 (covers none): document whether the ptrace CI host runs an LSM that can refuse `PROT_EXEC`. The refusal itself (F11) is host configuration, covered by U2, so this item no longer claims F11.
- U1 (covers F12; host-transient): the strict-overcommit commit charge depends on what every other host process has committed. The plain passthrough (`detcore/src/syscall_classification.rs:706`, `detcore/src/lib.rs:2843`) hands the `ENOMEM` to the guest today. F12 counts toward done only once Hermit hides this error or reports it as an infrastructure error.
- U2 (covers F11; host-config): whether an LSM refuses the change is fixed by the CI host's security configuration, which CI cannot vary. Hermit forwards the kernel's `EACCES` unchanged (`detcore/src/syscall_classification.rs:706`, `detcore/src/lib.rs:2843`).
- U3 (covers F7; host-config): the host's `vm.max_map_count` decides the threshold, and at the measurement host 67108864 a guest cannot create enough VMAs to reach it. Hermit forwards the kernel's `ENOMEM` unchanged (`detcore/src/syscall_classification.rs:706`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional, target_policy investigate -> passthrough (classified PassThrough at detcore/src/syscall_classification.rs:706 and forwarded at detcore/src/lib.rs:2843; Hermit has no page-protection model); listed 11 flag behaviours and 8 tests; no coverage token is recorded because no test calls SYS_mprotect directly, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (vm.max_map_count, vm.overcommit_memory, LSM policy); F7 narrowed to the map-count ENOMEM; new F12, the strict-overcommit ENOMEM (host-transient U1); host-config U2 for the LSM EACCES of F11 and U3 for the map-count ENOMEM of F7; T5 and T8 now cover none; the open question on unreachable host behaviour is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mprotect`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mprotect&body=Discussion+for+the+%60mprotect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmprotect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mprotect%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `mprotect`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
