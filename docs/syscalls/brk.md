---
schema: syscall-pages/v1
syscall: brk
nr_x86_64: 12
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1936
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+brk&body=Discussion+for+the+%60brk%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fbrk.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60brk%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 5
---

## Summary

`brk(addr)` moves the end of the process's data segment (the program break) and returns the break in effect afterwards. Hermit forwards it to the host kernel and only observes the result, to record where the guest heap is; the returned addresses repeat because Hermit disables address-space randomization and serializes the guest.

## Linux semantics

The raw syscall never returns an errno. It returns the new break on success and the unchanged current break on any failure; glibc's `brk()` turns a returned value below the request into `-1`/`ENOMEM`, and `sbrk()` is built on top of it. The initial break sits just past the end of the program's bss, randomized by a page offset unless randomization is disabled. Growing the break maps zero-filled anonymous memory; shrinking unmaps it. The break belongs to the address space, so all threads share it, `fork` copies it and `execve` resets it.

### Flags and options

- F1: query: `addr` of 0, or any value below the start of the data segment, changes nothing and returns the current break
- F2: grow: an `addr` above the current break extends the heap (page granular internally, but the exact requested value is returned and recorded as the break); new pages read as zero
- F3: shrink: an `addr` between the start of the data segment and the current break releases the pages above the new page-rounded break
- F4: failure: the kernel returns the old break unchanged; glibc `brk()` reports `ENOMEM` and `sbrk()` returns `(void *)-1`
- F5: the soft `RLIMIT_DATA` in force caps the heap size (together with other private writable mappings); a grow past it fails as F4. Under owner decision 5 the limit in force is the one `getrlimit` reports to the guest
- F6: a grow that would overlap the next mapping, or come within the stack guard gap, fails as F4
- F7: a grow larger than the host's RAM plus swap, which the default heuristic overcommit mode (`vm.overcommit_memory=0`) refuses, fails as F4
- F8: initial placement: the first break follows the ELF bss end and is randomized by `randomize_va_space=2` unless the process runs with `ADDR_NO_RANDOMIZE`
- F9: sharing: all threads of a process see one break; `fork` children inherit the parent's break and heap; `execve` sets a new initial break
- F10: under strict overcommit (`vm.overcommit_memory=2`), a grow fails as F4 once the system-wide commit charge, which every other host process adds to, reaches the commit limit
- F11: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). When the host's real `RLIMIT_DATA` is below the one the guest was told, a grow it refuses is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=brk errno=ENOMEM` (the errno glibc derives; the raw call returns the old break), not handed to the guest

## Determinism analysis

The result is a function of the executable's layout, the randomization setting, the mappings already in the address space, the host `RLIMIT_DATA` and the overcommit policy. Hermit runs the guest with `ADDR_NO_RANDOMIZE` (stated at `tests/c/personality_domain.c:12`), so F8 gives the same initial break for the same binary on the same host, and serialized scheduling gives the same order of `brk` and `mmap` calls, so F2, F3 and F6 repeat. The call repeats under those conditions, which is why `linux_determinism` is conditional.

### Nondeterminism sources

- Host randomization when it is not disabled (another backend, or a host that refuses the personality change).
- The host `RLIMIT_DATA` at launch. The guest is told the limit is infinite: Detcore's default virtual limits are `RLIM64_INFINITY` (`detcore/src/tool_local.rs:278`), changing `RLIMIT_DATA` returns `EPERM` (`detcore/src/syscalls/sysinfo.rs:147`), and the virtual limits are not enforced by the host kernel (`detcore/src/syscalls/sysinfo.rs:215`). A host with a finite data limit therefore makes F5 fail at a point the guest cannot query (F11).
- Host overcommit policy and memory pressure behind F7.
- Placement of other mappings by a different kernel version, which can move the F6 collision point.

### Cross-host variation

- Host `RLIMIT_DATA` inherited at launch: where a grow starts to fail (F5, F11). The guest is told the limit is infinite and cannot read the real one.
- Overcommit sysctl `vm.overcommit_memory` and the host's RAM plus swap: whether and at what size a large grow is refused (F7); mode 1 never refuses, mode 2 brings in F10.
- Kernel version: placement of the initial break and of neighbouring mappings, which moves the F8 starting address and the F6 collision point.

## Hermit design

Target policy: `passthrough`. The code already does this. `brk` is classified `PassThrough` at `detcore/src/syscall_classification.rs:696`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`, whose comment at `detcore/src/syscall_classification.rs:691` gives the condition ("conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions"); it is in the census at `detcore/src/syscall_classification.rs:1681`. The blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it.

After the host returns, Detcore observes the result. The comment at `detcore/src/lib.rs:2882` says "brk is PassThrough, so nothing else records where the guest's heap is", and the code below it calls `observe_brk` for every positive result. `observe_brk` (`detcore/src/memory.rs:77`) fixes the heap base from the first observed break, and `brk_heap_range` (`detcore/src/memory.rs:94`) gives the range used to log the heap on backends where the kernel does not label `[heap]`; unit tests cover it at `detcore/src/memory.rs:247`. This observation does not change the guest-visible result.

The 2026-08-07 strategy draft (kept under Additional information) asked for a heap base and growth policy chosen by a deterministic virtual-memory map, translated across backends. That is rejected for this tracking issue:

- Tracking is now judged on ptrace, where the guest runs in its own real address space. Choosing a virtual break would require Hermit to place and move the heap itself, that is, to emulate `brk` on top of `mmap`, while glibc's allocator and the kernel's own `mmap` placement continue to see the real break.
- Hermit has no virtual-memory map that owns placement; `mmap` and friends have typed handlers (`detcore/src/lib.rs:2403`) but placement remains the kernel's.
- With randomization off and the guest serialized, the forwarded result already repeats on a fixed host. The remaining host dependency, the data limit, is handled by owner decision 5 (below) rather than by replacing the kernel's break.

Target for the data limit (owner decision 5, 2026-10-04: limits become real and deterministic): the `RLIMIT_DATA` the guest reads is the limit in force, so F5 fails exactly at the reported limit. Hermit gets there per resource, either by configuring the guest's host `RLIMIT_DATA` to the reported value at launch and on each accepted `setrlimit`/`prlimit64`, which keeps `brk` a passthrough, or by enforcing the reported limit in Detcore, which would have to count every private writable mapping as the kernel does. The configure route fits this body's `passthrough` policy; this body does not rule out the other.

Interim, until `RLIMIT_DATA` reaches the target (F11): a grow refused by a host limit that the reported limit would not predict is a determinism failure, not a guest failure, and is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=brk errno=ENOMEM` through the infrastructure-error convention. Today the unchanged break reaches the guest through the plain passthrough (`detcore/src/syscall_classification.rs:696`, `detcore/src/lib.rs:2843`); that is the gap. Detcore already sees every result at `detcore/src/lib.rs:2882`, so a returned break below the request is visible there; telling F11 apart from F6, F7 and F10 needs the host limit, which Hermit can read from the launcher at start-up.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): `RLIMIT_DATA` is reported as one fixed value with soft equal to hard, and it stays `RLIM_INFINITY`, so no realistic guest's heap is cut short. Hermit configures the guest's host limit to it at launch, which keeps `brk` a passthrough. A guest lowering it through `setrlimit` or `prlimit64` is forwarded to the host and recorded, so T3's grow fails at exactly the limit the guest set; a raise of the hard limit is `EPERM`. A host hard limit below the reported value cannot be configured away; it is the `cause=host-rlimit` infrastructure error (F11), detected once at launch rather than at the failed grow.

Code facts show `subscribed_record_replay: false`: `brk` is not in the `--passthru-opt` list near `detcore/src/lib.rs:1214`. `hermit record` uses its own subscriptions, in a recorder module the comment at `detcore/src/lib.rs:1078` places outside the exported tree; this body does not describe how record treats `brk`.

The open gap is test evidence. `tests_naming` is 0: existing cells reach `brk` through glibc `sbrk()` and `brk()`.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call raw `syscall(SYS_brk, 0)`, grow by one page plus a few bytes, write and read back the new memory (expect zeros before the write), shrink back by a page, and query again. Print every returned break as an offset from the first one and the first one itself.
  Existing partial coverage via glibc: `tests/c/mmap_determinism.c:69` (`heap_growth`: `sbrk(0)`, `sbrk(page)`, `brk(after + 2*page)`; cell `c-programs/mmap-determinism-heap`, `tests/e2e/manifests/c-programs.yaml:17691`, ptrace verify in CI, printing the absolute addresses so the verify comparison pins them).
- T2 (covers F4, F6): place a fixed `mmap` a few pages above the current break, grow the break into it, and assert the raw syscall returns the old break and glibc `brk()` returns `-1` with `ENOMEM`.
- T3 (covers F5): under `hermit run --verify` on ptrace, the guest lowers its own soft `RLIMIT_DATA` with `setrlimit` to its current data size plus 1 MiB, grows the break by 512 KiB (succeeds) and then by 1 MiB more, and asserts the second grow fails as in T2, exactly at the limit it set. Fails at this SHA: the `setrlimit` returns `EPERM` (`detcore/src/syscalls/sysinfo.rs:147`).
- T4 (covers F7): request a grow larger than physical memory plus swap and assert the old break is returned. The test must state whether the host's overcommit mode makes this reachable in ptrace CI.
- T5 (covers F8): run the same binary twice under `hermit run --verify` and assert the first break is identical in both runs; add a check that the process personality includes `ADDR_NO_RANDOMIZE`.
  Existing partial coverage: `tests/c/mmap_layout_pointer_order.c:190` reads `sbrk(0)` as part of a layout probe (cell `c-programs/mmap-layout-pointer-order`, `tests/e2e/manifests/c-programs.yaml:16690`, ptrace verify in CI).
- T6 (covers F9): grow the break in one thread and read it in another; `fork` and check that the child's break equals the parent's and that heap contents were copied; `execve` a helper and check its first break matches a direct launch of the helper.
- T7 (covers F11): launch Hermit under a lowered host data limit (`prlimit --data=<limit>` on the launcher, with the limit a few MiB above what Hermit and the guest need to start). The guest reads `RLIMIT_DATA` (infinite) and grows the break by 64 MiB. Assert that the run reports `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=brk errno=ENOMEM` and ends with the infrastructure-error exit convention (exit 122 with `class=infrastructure-error cause=host-rlimit` on stderr, as proposed in the error-handling convention), not with glibc `brk()` returning `-1`. A report at launch, when Hermit cannot configure the reported limit, also satisfies the test. Today the guest sees the failed grow.
- U1 (covers F10; host-transient): under strict overcommit the commit charge is shared with every other host process, so a grow can fail or succeed depending on what else runs. Hermit forwards `brk` unchanged (`detcore/src/syscall_classification.rs:696`, `detcore/src/lib.rs:2843`) and hands the kernel's unchanged break to the guest today. F10 counts toward done only once Hermit hides this failure or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. brk is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1936: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional, target_policy investigate -> passthrough (forwarded at detcore/src/lib.rs:2843 and only observed at detcore/src/lib.rs:2882; the legacy virtual-heap draft is rejected for ptrace); listed 9 flag behaviours and 6 tests; open question on the host RLIMIT_DATA ceiling versus the infinite virtual limit; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host RLIMIT_DATA, vm.overcommit_memory and RAM plus swap, kernel version); F7 narrowed to the heuristic-mode refusal; new F10, strict-overcommit commit-limit failure (host-transient U1); the open question on strict overcommit is resolved by decision 1
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (reported RLIMIT_DATA is the limit in force, configured on the host or enforced by Detcore; interim host-rlimit infrastructure error, passed through today at syscall_classification.rs:696 and lib.rs:2843); target_policy stays passthrough (the configure route keeps the call forwarded); T3 now has the guest lower its own limit; new F11, T7; removed the RLIMIT_DATA open question; added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (RLIMIT_DATA fixed with soft equal to hard at RLIM_INFINITY, configured on the host at launch; a lowering is forwarded and recorded; a hard raise is EPERM; a host hard limit below it is cause=host-rlimit at launch, F11); target_policy stays passthrough; T3 unchanged; removed the standing-policy proposal and the empty Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `brk`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+brk&body=Discussion+for+the+%60brk%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fbrk.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60brk%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: brk" (https://github.com/rrnewton/hermit/issues/1936), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy strategy draft (2026-08-07)

Integrate brk with Hermit's deterministic virtual-memory map. Choose the heap base and growth policy from deterministic state, serialize mapping conflicts, and translate the virtual result consistently across backends; use deterministic refusal for unsupported layouts.

Legacy required tests (2026-08-07):

- Exercise grow, shrink, no-op query, limit exhaustion, and collision with an adjacent mapping.
- Assert identical returned break addresses across repeated strict runs and record/replay.
- Interleave brk with mmap, munmap, clone, and exec cases under the deterministic scheduler.
- Cover ptrace and every backend claiming support, with a planted host-layout divergence refused by the gate.

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

brk is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
