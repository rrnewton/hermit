---
schema: syscall-pages/v1
syscall: pkey_free
nr_x86_64: 331
linux_determinism: deterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/323
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pkey_free&body=Discussion+for+the+%60pkey_free%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpkey_free.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pkey_free%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`pkey_free(pkey)` returns a memory protection key to the calling process's pool so a later `pkey_alloc` can hand it out again. Hermit forwards it to the host kernel unchanged; the result depends only on which keys the process has allocated.

## Linux semantics

Pairs with `pkey_alloc`. The kernel clears the key's bit in the per-address-space allocation bitmap under the mmap write lock and returns 0, or returns `EINVAL` if the key is not currently allocated. It does nothing else: it does not change any thread's PKRU register and does not untag pages that still carry the key. It takes no flags. The behaviours below are the argument-dependent cases. The x86 rules for which keys count as allocated are in `mm_pkey_is_allocated` in the kernel's `arch/x86/include/asm/pkeys.h`, read from the host's kernel headers for this body.

### Flags and options

- F1: `pkey` was returned by `pkey_alloc` and is still allocated: returns 0 and the key becomes free. Only a CPU with PKU ever returns a key; on a CPU without PKU, which Hermit's target emulates (owner decision 8), `pkey_alloc` returns none, so the only key that can be freed is 0 (F4)
- F2: `pkey` is negative, at least the number of usable keys (16 with PKU, 1 without), or not currently allocated, including a second free of the same key: `EINVAL`. Without PKU this is every key except 0 while key 0 is still allocated
- F3: `pkey` is the key the kernel reserved for execute-only mappings (taken the first time the process maps memory with `PROT_EXEC` alone): `EINVAL`, even though its bit is set. This key exists only on a CPU with PKU; without PKU any such number is out of range and gets `EINVAL` by F2
- F4: `pkey == 0`: the kernel marks key 0 allocated when the address space is created, so by the x86 helper `pkey_free(0)` returns 0 and a second `pkey_free(0)` gives `EINVAL`. On a CPU with PKU a later `pkey_alloc` can return 0; without PKU that `pkey_alloc` fails with `EINVAL` instead (see `pkey_alloc` F10)
- F5: freeing a key that pages are still tagged with leaves them tagged and changes no PKRU; on a CPU with PKU a later `pkey_alloc` may return the same number, and those pages then follow the new owner's access rights. Without PKU no key is re-issued, so pages tagged with key 0 stay accessible as before
- F6: scope: the bitmap belongs to the address space, so a free by one thread is seen by all threads, and after `fork` the parent and child free keys independently

## Determinism analysis

The result is a function of the process's own earlier `pkey_alloc`, `pkey_free` and execute-only mappings. On a host without PKU only key 0 exists, so every other key gives `EINVAL`. The call does not depend on timing or on other processes, and Hermit runs guest threads one at a time, so frees and allocations racing between threads happen in a fixed order.

### Nondeterminism sources

- None run to run on one host; per-process key-pool bookkeeping only.
- Across hosts, F2 depends on PKU support: key 1 can be freed after a successful allocation on a PKU host, while on a host without PKU the allocation already failed and the free gives `EINVAL`.

### Cross-host variation

- CPU feature PKU, and the `nopku` boot parameter: the number of usable keys (16 or 1), so the same allocate-then-free sequence gives 0 (F1) on a PKU host and `EINVAL` (F2) on a host without PKU.

## Hermit design

Current behaviour (hermit 239ca43d3092): the call still passes through, but through a Detcore wrapper instead of the blanket arm. `handle_protection_key_call` (`detcore/src/lib.rs:1764`) forwards the call, records successful key allocation or application in thread state (rendered `/proc` memory views depend on knowing keys are in use), and names a backend refusal of protection keys instead of surfacing a bare errno. The classification remains `passthrough` with a dedicated dispatch arm. The `emulate` target below (present a CPU without protection keys, matching the virtual CPUID) is still the intended policy and still unrealised inside Detcore itself, so on the default backend a host with protection keys still answers with real keys.

Today the call is passed through at hermit f4055eb9cae4. `pkey_free` is classified `PassThrough` at `detcore/src/syscall_classification.rs:822`, in the issue-reviewed pass-through block that starts at `detcore/src/syscall_classification.rs:807`. The generic arm at `detcore/src/lib.rs:2843` forwards it through `passthrough` (`detcore/src/lib.rs:367`). The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the `PassThrough` class at `detcore/src/syscall_classification.rs:2111` and `detcore/src/syscall_classification.rs:2119`.

Target (owner decision 8): Detcore emulates a CPU without PKU, matching its virtual CPUID, whose leaf 7 ECX = 0 hides PKU and OSPKE (`detcore/src/cpuid.rs:66`), and never forwards the call. Without OSPKE, `arch_max_pkey()` is 1 and no execute-only key is ever reserved, so `mm_pkey_is_allocated` (`arch/x86/include/asm/pkeys.h`, called from `mm_pkey_free` in `SYSCALL_DEFINE1(pkey_free, ...)`, `mm/mprotect.c`) checks, in order: `pkey < 0`, `pkey >= 1`, then the key-0 bit. Detcore keeps that bit per address space, shared with `pkey_alloc` and `pkey_mprotect`: set at creation and at `execve`, copied on `fork`. `pkey_free(0)` with the bit set clears it and returns 0 (F4); every other call returns `EINVAL` (F2, F3). Today's passthrough frees real keys on a PKU host, which the virtual CPUID says cannot exist; emulating only the free while `pkey_alloc` still reached the kernel would desynchronize the two, so the three calls change together.

Code facts show `subscribed_record_replay: false`, so with `--passthru-opt` the call never reaches Detcore. The recorder's own subscriptions are not in the exported tree.

The open gap is test evidence: no test at this SHA names `pkey_free` (`tests_naming: 0`).

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call raw `syscall(SYS_pkey_free, ...)`. Assert that `pkey_alloc(0, 0)` returns `ENOSPC`, so no key exists to free, and that `pkey_free` returns `EINVAL` for -1, 1, 15 and 16, identically on a PKU host and on a host without PKU. The test prints whether the host's `/proc/cpuinfo` lists `pku`.
- T2 (covers F3): map a page with `PROT_EXEC` alone, read its key from the `ProtectionKey:` line of `/proc/self/smaps` where the line is present, and assert that `pkey_free` of that key, when it is not 0, and of key 1 returns `EINVAL`.
- T3 (covers F4): call `pkey_free(0)`, then `pkey_alloc(0, 0)`, then `pkey_free(0)` again. Assert 0, `EINVAL`, `EINVAL` under Hermit on any host, and that both `--verify` runs agree. The expected values come from the kernel source read for this body (`mm_pkey_is_allocated`, `arch_set_user_pkey_access`); run the program natively once on a kernel booted with `nopku` to confirm them.
- T4 (covers F5): tag a page with key 0 through `pkey_mprotect`, call `pkey_free(0)`, and assert that the page is still readable and writable and that `pkey_mprotect` of it with key -1 still succeeds.
- T5 (covers F6): `fork`. The child calls `pkey_free(0)` and asserts 0. The parent then asserts that its own `pkey_free(0)` also returns 0, because its bitmap still has key 0. In a two-thread process, a `pkey_free(0)` by one thread makes the other thread's `pkey_free(0)` return `EINVAL`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/323#issuecomment-5872914123
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/323: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pkey_free records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/323 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843; the key pool is kernel state shared with pkey_alloc and pkey_mprotect); listed 6 argument-dependent behaviours and 5 tests; no test names pkey_free, so tracking_status stays todo; follows the pkey_alloc CPUID question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (CPU PKU and nopku); no U items, since the free itself is decided by the process's key bitmap; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 8 and 6 applied (emulate a CPU without PKU: only key 0 can be freed, checks in mm_pkey_is_allocated order); target_policy passthrough->emulate; F1-F5 narrowed to the no-PKU target, T1-T5 rewritten to match; no new F/T ids; removed the pkey CPUID question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated: protection-key calls now pass through a Detcore wrapper that records key use and names backend refusals; target stays emulate.

## Discussion

[Start a discussion about `pkey_free`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pkey_free&body=Discussion+for+the+%60pkey_free%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpkey_free.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pkey_free%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pkey_free" (https://github.com/rrnewton/hermit/issues/323), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/323.

### Open questions

- F4 comes from reading the kernel's x86 header, not from a run. T3 confirms or refutes it.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pkey_free`, and by confirming hermit currently routes `pkey_free` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pkey_free(1)                    = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/323#issuecomment-5872914123
