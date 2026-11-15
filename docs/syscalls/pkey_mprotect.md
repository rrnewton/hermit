---
schema: syscall-pages/v1
syscall: pkey_mprotect
nr_x86_64: 329
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/324
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pkey_mprotect&body=Discussion+for+the+%60pkey_mprotect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpkey_mprotect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pkey_mprotect%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`pkey_mprotect(addr, len, prot, pkey)` changes the protection of a memory range like `mprotect` and also tags the range with a memory protection key. Hermit forwards it to the host kernel unchanged. With `pkey == -1` it behaves exactly like `mprotect`. With any other key the result depends on whether the host CPU and kernel support protection keys (Intel/AMD PKU), so the rating is conditional on the host.

## Linux semantics

The call was added in Linux 4.9. The kernel shares `mprotect`'s implementation (`do_mprotect_pkey`). It requires a page-aligned `addr`, rounds `len` up to whole pages, validates `prot` and the key, then rewrites the protection and key of every VMA in the range. A key other than -1 must already be allocated in the process, either by `pkey_alloc` or as the default key 0. The key only takes effect through the per-thread PKRU register, which user space changes with the unprivileged `WRPKRU` instruction. An access that PKRU forbids raises `SIGSEGV` with `si_code SEGV_PKUERR`. On a host without PKU, at most key 0 counts as allocated. The call writes no memory in the caller.

### Flags and options

- F1: `pkey == -1`: identical to `mprotect(addr, len, prot)`. The range keeps its current key, except as F6 says.
- F2: `pkey == 0`: the default key, allocated when the address space is created, including on hosts without PKU. The range is reset to key 0. After a successful `pkey_free(0)` key 0 is no longer allocated and gives `EINVAL` as F4 says
- F3: `pkey` from a successful `pkey_alloc` (PKU hosts only): tag the range with that key, so later accesses are checked against the thread's PKRU bits for it. On a CPU without PKU, which Hermit's target emulates (owner decision 8), `pkey_alloc` never returns a key, so this case does not arise and every such number falls under F4
- F4: an unallocated or out-of-range key gives `EINVAL`. On a host without PKU every key other than -1 and 0 gives `EINVAL`.
- F5: `prot` bits: any combination of `PROT_READ`, `PROT_WRITE` and `PROT_EXEC`, or `PROT_NONE`. `PROT_SEM` is accepted and has no effect on x86. Unknown bits give `EINVAL`.
- F6: execute-only memory: on a PKU host, `prot == PROT_EXEC` with `pkey == -1` makes the range execute-only by using a key the kernel allocates for the process on first use. That consumes one key from the same pool `pkey_alloc` draws from, so later `pkey_alloc` results shift. On a host without PKU, the CPU the target emulates, the range stays readable and no key is consumed.
- F7: `PROT_GROWSDOWN` / `PROT_GROWSUP`: extend the change to the start or end of a growable stack mapping. Both together give `EINVAL`, as does `PROT_GROWSDOWN` on a mapping that is not grows-down. `PROT_GROWSUP` is not supported on x86 and gives `EINVAL`.
- F8: range: an `addr` that is not page-aligned gives `EINVAL`. `len == 0` succeeds without doing anything. A range with an unmapped hole, or one that wraps the address space, gives `ENOMEM`.
- F9: mapping permissions: adding `PROT_WRITE` to a shared file mapping opened read-only, or `PROT_EXEC` to a mapping from a `noexec` mount, gives `EACCES`. A sealed mapping (`mseal`, Linux 6.10+) gives `EPERM`.
- F10: a change that would split VMAs beyond the host's `vm.max_map_count` gives `ENOMEM`.
- F11: check order, from `do_mprotect_pkey` in `mm/mprotect.c`: `PROT_GROWSDOWN` and `PROT_GROWSUP` together (`EINVAL`), a misaligned `addr` (`EINVAL`), `len == 0` (returns 0), a wrapping range (`ENOMEM`), unknown `prot` bits (`EINVAL`), and only then the key (F4), before any VMA is looked at. So `len == 0` with an unallocated key returns 0, and a misaligned `addr` gives `EINVAL` whatever the key

## Determinism analysis

With `pkey` -1 or 0 and no execute-only request, the result is a function of the guest's arguments and its own address-space layout, the same as `mprotect`, and it repeats. With any other key, and for `PROT_EXEC`-only ranges, it also depends on whether the host has PKU. A PKU host accepts allocated keys and makes execute-only memory. A non-PKU host refuses every key other than -1 and 0 with `EINVAL` and leaves execute-only ranges readable. On one host the outcome repeats; across hosts it differs. That is why the rating moved from deterministic to conditional.

### Nondeterminism sources

- Host PKU support (`X86_FEATURE_OSPKE`) decides F3, F4 and F6.
- The execute-only key in F6 is allocated from the process's key pool, so whether a `PROT_EXEC`-only call happened before a `pkey_alloc` changes the key number `pkey_alloc` returns. That is guest-ordered, so it repeats on one host.
- `/proc/self/smaps` `ProtectionKey` shows the key of each mapping on PKU hosts and is absent on others (see Hermit design).
- None from timing: Detcore charges every syscall a fixed virtual cost (`detcore/src/lib.rs:1936`), and `pkey_mprotect` falls back to `detcore/src/syscall_time.rs:156`.

### Cross-host variation

- CPU feature PKU (`X86_FEATURE_OSPKE`), and the `nopku` boot parameter: whether allocated keys can be applied (F3), which keys give `EINVAL` (F4), whether a `PROT_EXEC`-only range becomes execute-only and consumes a key (F6), and whether `/proc/self/smaps` shows `ProtectionKey`.
- Sysctl `vm.max_map_count`: how many VMA splits succeed before `ENOMEM` (F10).

## Hermit design

Current behaviour (hermit 239ca43d3092): the call still passes through, but through a Detcore wrapper instead of the blanket arm. `handle_protection_key_call` (`detcore/src/lib.rs:1764`) forwards the call, records successful key allocation or application in thread state (rendered `/proc` memory views depend on knowing keys are in use), and names a backend refusal of protection keys instead of surfacing a bare errno. The classification remains `passthrough` with a dedicated dispatch arm. The `emulate` target below (present a CPU without protection keys, matching the virtual CPUID) is still the intended policy and still unrealised inside Detcore itself, so on the default backend a host with protection keys still answers with real keys.

Today the call is passed through. `pkey_mprotect` is classified `PassThrough` at `detcore/src/syscall_classification.rs:823`, alongside `pkey_alloc` (`detcore/src/syscall_classification.rs:821`) and `mprotect` (`detcore/src/syscall_classification.rs:706`). It sits in the issue-reviewed pass-through block (`detcore/src/syscall_classification.rs:807`), whose comment says its members "remain conditional on Hermit's fixed-container and stable-state assumptions" (`detcore/src/syscall_classification.rs:809`), under `TODO-HUMAN-REVIEW(PR-643)` (`detcore/src/syscall_classification.rs:811`). The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards the call, and `representative_policies_stay_in_their_reviewed_sections` asserts the classification at `detcore/src/syscall_classification.rs:2112`.

Two Hermit details interact with protection keys:

- CPUID hides PKU. When CPUID virtualization is enabled, Detcore traps `cpuid` (`detcore/src/lib.rs:1096`) and answers from a fixed table (`InterceptedCpuid::cpuid`, `detcore/src/cpuid.rs:23`). Leaf 7 in that table has ECX = 0 (`detcore/src/cpuid.rs:66`), so the guest sees neither PKU nor OSPKE. Programs that check CPUID before using keys will skip them. But `pkey_alloc` and `pkey_mprotect` still reach the host kernel, which accepts keys on a PKU host, and `RDPKRU`/`WRPKRU` still work, because CPUID faulting does not disable instructions. A guest that calls the syscalls without checking CPUID therefore sees PKU behaviour that its CPUID denies, and that behaviour differs between hosts.
- `/proc/self/smaps` passes `ProtectionKey` through verbatim (`detcore/src/procfs.rs:3035`, copied at `detcore/src/procfs.rs:3044`). On one host that value depends only on the guest's own calls.

Target (owner decision 8): emulate a CPU without PKU, matching the CPUID above, so the result no longer depends on the host's PKU support. Detcore keeps one bit per address space, whether key 0 is still allocated, shared with `pkey_alloc` and `pkey_free` (set at creation and at `execve`, copied on `fork`, cleared by `pkey_free(0)`). Then:

- `pkey == -1`, or `pkey == 0` while the bit is set: forward to the kernel, which acts as `mprotect` (F1, F2, F5, F7 to F10). When `prot` is `PROT_EXEC` alone, Detcore forwards key 0 instead of -1, because on a PKU host the kernel's `__arch_override_mprotect_pkey` would otherwise allocate the execute-only key and make the range unreadable (F6); an explicit key is never overridden. `mprotect` and `mmap` with `PROT_EXEC` alone reach the same kernel path and are outside this body.
- any other key: Detcore answers itself and does not forward. It applies the checks that `do_mprotect_pkey` runs before the key, in that order (owner decision 6, F11), and returns their result; if all pass, it returns `EINVAL` (F4).

Programs that use keys already handle a CPU without PKU, because `pkey_alloc` fails there first (`ENOSPC`, see the `pkey_alloc` body); glibc's `pkey_mprotect` wrapper turns key -1 into `mprotect`, which this target leaves unchanged.

## Recommended tests

- T1 (covers F1, F2, F5, F8): under `hermit run --verify` on ptrace, use raw `syscall(SYS_pkey_mprotect, ...)`, because glibc's `pkey_mprotect` wrapper calls `mprotect` when `pkey == -1`. On a 2-page anonymous `mmap`, apply `PROT_READ`, `PROT_READ|PROT_WRITE`, `PROT_NONE` and `PROT_READ|PROT_SEM` with keys -1 and 0, and assert 0 each time. Write and read back through the mapping where `prot` allows it. Assert `EINVAL` for a misaligned `addr` and for an unknown `prot` bit (`0x100`), 0 for `len == 0`, and `ENOMEM` for a range covering an unmapped page.
  Existing coverage: none; no test at the cited SHA names `pkey_mprotect`.
- T2 (covers F3, F4, F6): assert that `pkey_alloc(0, 0)` returns `ENOSPC`, then that `pkey_mprotect` of a mapped page with keys 1 and 15 gives `EINVAL`. Apply `PROT_EXEC` alone with key -1 to a page and assert that a read of it still succeeds and that its `smaps` `ProtectionKey` line, where present, shows 0. The results must be the same on a PKU host and on a host without PKU; the test prints whether the host's `/proc/cpuinfo` lists `pku`.
- T3 (covers F7, F9): assert `EINVAL` for `PROT_GROWSDOWN|PROT_GROWSUP`, for `PROT_GROWSUP` alone, and for `PROT_GROWSDOWN` on an ordinary anonymous mapping. Map a file opened `O_RDONLY` with `MAP_SHARED|PROT_READ` and assert that adding `PROT_WRITE` gives `EACCES`.
- T4 (covers F11): with key 5 (unallocated), assert 0 for `len == 0`, `EINVAL` for a misaligned `addr`, `ENOMEM` for a range that wraps the address space, and `EINVAL` for `prot` with bit `0x100` and for `PROT_GROWSDOWN|PROT_GROWSUP`. None of these may be the key's `EINVAL` reported early: the 0 and the `ENOMEM` show that the key check comes after them.
- T5 (covers F2): call `pkey_free(0)` and assert 0; then assert that `pkey_mprotect` of a mapped page with key 0 gives `EINVAL` and with key -1 still returns 0.
- U1 (covers F10; host-config): the host's `vm.max_map_count` decides the threshold, and at the measurement host 67108864 a guest cannot create enough VMAs to reach it. Hermit forwards the kernel's `ENOMEM` unchanged (`detcore/src/syscall_classification.rs:823`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/324#issuecomment-5872916536
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/324: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pkey_mprotect records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/324 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic -> conditional (allocated keys and execute-only PROT_EXEC depend on host PKU support); kept passthrough (classified at detcore/src/syscall_classification.rs:823); 9 flag behaviours and 3 tests listed, no existing test; an open question (Hermit's CPUID leaf 7 hides PKU at detcore/src/cpuid.rs:66 while the host kernel still grants keys) keeps tracking_status todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (CPU PKU and nopku, vm.max_map_count); F8 narrowed to the hole and wrap ENOMEM; new F10, the max_map_count ENOMEM (host-config U1); no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 8 and 6 applied (emulate a CPU without PKU: keys -1 and an allocated 0 act as mprotect, PROT_EXEC-only forwarded with key 0, any other key EINVAL after do_mprotect_pkey's earlier checks); target_policy passthrough->emulate; F2, F3, F6 narrowed; T2 rewritten for the no-PKU target; new F11, T4, T5; removed the CPUID question and the PKU-CI-host question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated: protection-key calls now pass through a Detcore wrapper that records key use and names backend refusals; target stays emulate.

## Discussion

[Start a discussion about `pkey_mprotect`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pkey_mprotect&body=Discussion+for+the+%60pkey_mprotect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpkey_mprotect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pkey_mprotect%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pkey_mprotect" (https://github.com/rrnewton/hermit/issues/324), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/324.

The legacy analysis rated the call deterministic because `pkey` is caller-controlled and the strace probe used `pkey == -1`. That holds for F1 and F2. It does not hold for allocated keys or execute-only memory, which this body rates conditional on the host.

### Open questions


### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pkey_mprotect`, and by confirming hermit currently routes `pkey_mprotect` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pkey_mprotect(0x406000, 4096, PROT_READ|PROT_WRITE, -1) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/324#issuecomment-5872916536
