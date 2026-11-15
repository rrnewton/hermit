---
schema: syscall-pages/v1
syscall: pkey_alloc
nr_x86_64: 330
linux_determinism: deterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/322
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pkey_alloc&body=Discussion+for+the+%60pkey_alloc%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpkey_alloc.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pkey_alloc%60%2C+or+in+what+the+page+says%3F%0A
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

`pkey_alloc(flags, access_rights)` allocates an x86 memory protection key (MPK/PKU) for the calling process and sets the calling thread's initial access rights for it in the PKRU register. Hermit forwards it to the host kernel unchanged; the answer depends only on the process's own key history and on whether the host CPU and kernel support protection keys.

## Linux semantics

The kernel keeps a per-address-space allocation bitmap of the 16 hardware keys. Key 0 is the default key and is marked allocated when the address space is created. A successful call marks the lowest clear bit, returns that key number, and writes the requested access-disable bits for that key into the calling thread's PKRU register. It returns no pointer and copies no guest memory. The argument checks (`flags`, then `access_rights`) run before the allocation, so `EINVAL` does not depend on the host's PKU support. On a host without PKU (or booted with `nopku`) the kernel reports only one usable key, the bitmap is already full, and every valid call fails with `ENOSPC`. A kernel built without protection-key support has no handler and returns `ENOSYS`.

### Flags and options

- F1: `flags` must be 0; any other value gives `EINVAL`
- F2: `access_rights == 0` (`PKEY_UNRESTRICTED`) is a valid value. On a CPU with PKU the calling thread may read and write pages tagged with the new key; on a CPU without PKU, which Hermit's target emulates (owner decision 8), the call passes the argument checks and fails as F10 says
- F3: `PKEY_DISABLE_ACCESS` (0x1) is a valid value. On a CPU with PKU the calling thread's PKRU denies every data access to pages tagged with the new key, and combined with `PKEY_DISABLE_WRITE` access-disable dominates; on a CPU without PKU the call passes the argument checks and fails as F10 says
- F4: `PKEY_DISABLE_WRITE` (0x2) is a valid value. On a CPU with PKU the calling thread's PKRU denies writes but allows reads; on a CPU without PKU the call passes the argument checks and fails as F10 says
- F5: any `access_rights` bit outside `PKEY_DISABLE_ACCESS | PKEY_DISABLE_WRITE` gives `EINVAL` on x86 (`PKEY_DISABLE_EXECUTE` exists only on powerpc)
- F6: the returned key is the lowest free key in 1..15. Keys already taken by earlier `pkey_alloc` calls count, and so does the key the kernel reserves the first time the process maps memory with `PROT_EXEC` alone (an execute-only mapping), so the number depends on the process's own earlier calls. This holds on a CPU with PKU only: without PKU the one key, 0, is allocated when the address space is created and no execute-only key exists, so `pkey_alloc` never returns a key (F10)
- F7: `ENOSPC` when all keys are allocated. Without PKU the bitmap holds key 0 only, so it is full from the start
- F8: scope: the allocation bitmap belongs to the address space, so threads share it, `fork` copies it and `execve` resets it to key 0 only. The access rights are written only to the calling thread's PKRU; other threads keep whatever bits they already had for that key, and new threads inherit their creator's PKRU. Without PKU the bitmap is the single key-0 bit, with the same scope: a `pkey_free(0)` by one thread is seen by all, `fork` copies it, `execve` sets it again
- F9: host-config errors: `ENOSPC` on every valid call when the CPU lacks PKU or the kernel has it disabled (`nopku`), and `ENOSYS` when the kernel was built without protection-key support (`CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS`). Under the target Detcore answers the call itself (F10), so neither host answer reaches the guest
- F10: CPU without PKU, the CPU that Hermit's virtual CPUID describes and the target emulates: after the F1 and F5 checks every call returns `ENOSPC`, allocates nothing and writes no PKRU. If the process has freed key 0 (`pkey_free(0)` returns 0 there), the kernel instead allocates key 0, `arch_set_user_pkey_access` (`arch/x86/kernel/fpu/xstate.c`) refuses it because OSPKE is off, the key is freed again, and the call returns `EINVAL`

## Determinism analysis

The result is a function of the process's own earlier key allocations, frees and execute-only mappings, plus one fixed host property: whether the CPU and kernel enable PKU. Nothing in the call depends on timing, other processes or the filesystem. Under Hermit, guest threads run one at a time, so two threads allocating at once get keys in a fixed order. On one host the call repeats exactly.

### Nondeterminism sources

- None run to run on one host.
- Across hosts, F9 differs: a PKU host returns a key, a host without PKU returns `ENOSPC`. This is a portability difference, not run-to-run nondeterminism. It matters for a recording replayed on another machine.
- The PKRU register written by this call is per-thread CPU state that the guest reads with `RDPKRU` and writes with `WRPKRU` without a syscall. Hermit does not see those instructions.

### Cross-host variation

- CPU feature PKU (with OSPKE), and the `nopku` boot parameter: whether valid calls return keys (F2 to F6) or `ENOSPC` every time (F9).
- Kernel build option `CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS`: without it the call gives `ENOSYS` (F9).

## Hermit design

Current behaviour (hermit 239ca43d3092): the call still passes through, but through a Detcore wrapper instead of the blanket arm. `handle_protection_key_call` (`detcore/src/lib.rs:1764`) forwards the call, records successful key allocation or application in thread state (rendered `/proc` memory views depend on knowing keys are in use), and names a backend refusal of protection keys instead of surfacing a bare errno. The classification remains `passthrough` with a dedicated dispatch arm. The `emulate` target below (present a CPU without protection keys, matching the virtual CPUID) is still the intended policy and still unrealised inside Detcore itself, so on the default backend a host with protection keys still answers with real keys.

Today the call is passed through at hermit f4055eb9cae4. `pkey_alloc` is classified `PassThrough` at `detcore/src/syscall_classification.rs:821`, in the issue-reviewed pass-through block that starts at `detcore/src/syscall_classification.rs:807`; its comment at `detcore/src/syscall_classification.rs:808` and `detcore/src/syscall_classification.rs:809` says these remain "conditional on Hermit's fixed-container and stable-state assumptions". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it, and `passthrough` at `detcore/src/lib.rs:367` sends it to the kernel through `record_or_replay_preserving_tool_errors`. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the `PassThrough` class for it at `detcore/src/syscall_classification.rs:2110` and `detcore/src/syscall_classification.rs:2119`.

Passthrough grants keys on a PKU host. That contradicts the virtual CPUID: when CPUID virtualization is on, Detcore answers CPUID from a fixed table (`detcore/src/lib.rs:1318`), and its leaf 7 entry at `detcore/src/cpuid.rs:66` has ECX = 0, so the PKU and OSPKE feature bits are clear and the guest is told protection keys do not exist. A guest that checks CPUID never calls `pkey_alloc`; a guest that calls it directly, as glibc's wrapper lets it, gets a key on a PKU host and `ENOSPC` elsewhere (F9).

Target (owner decision 8): Detcore emulates a CPU without PKU and never forwards the call. It applies the kernel's argument checks in the kernel's order (owner decision 6), as `SYSCALL_DEFINE2(pkey_alloc, ...)` in `mm/mprotect.c` runs them: `flags != 0` gives `EINVAL` (F1), then a bit outside `PKEY_ACCESS_MASK` gives `EINVAL` (F5). It then returns `ENOSPC`, because on x86 without OSPKE `arch_max_pkey()` is 1 and key 0, allocated at address-space creation, fills the bitmap (`mm_pkey_alloc` in `arch/x86/include/asm/pkeys.h`). No key is issued and no PKRU is written. To match F10 exactly, Detcore keeps one bit per address space, whether key 0 is still allocated, shared with `pkey_free` and `pkey_mprotect`: set at creation and at `execve`, copied on `fork`, cleared by a successful `pkey_free(0)`; while it is clear the answer is `EINVAL`. This is the answer Linux gives on any x86 CPU or VM without PKU, so programs that use keys already handle it; V8, for example, keeps its non-PKU code-space protection when `pkey_alloc` fails. Once this lands, the F9 difference between hosts no longer reaches the guest; `cross_host_variant` stays true until then, because it describes today's code.

Code facts show `subscribed_record_replay: false`, so with `--passthru-opt` the call never reaches Detcore. `hermit record` has its own subscriptions in a recorder module that is not in the exported tree, so this body does not describe how record captures the call.

The open gap is test evidence: no test at this SHA names `pkey_alloc` (`tests_naming: 0`).

## Recommended tests

- T1 (covers F1, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_pkey_alloc, ...)` with `flags = 1`, with `access_rights = 0x4`, and with `access_rights = ~0`. Assert `EINVAL` for each. These checks run before allocation, so they hold on hosts with and without PKU.
- T2 (covers F2, F3, F4, F6): in a fresh process, call `pkey_alloc` with `0`, `PKEY_DISABLE_ACCESS`, `PKEY_DISABLE_WRITE` and both bits. Assert `ENOSPC` for each (not `EINVAL`, so the value passed the argument check), identically in both `--verify` runs, on a PKU host as on a host without PKU. The test prints whether the host's `/proc/cpuinfo` lists `pku`, so a run on a PKU host, where today's passthrough returns keys, is visible.
- T3 (covers F6, F7): call `pkey_alloc(0, 0)` 16 times and assert `ENOSPC` every time. In a second process, first map a page with `PROT_EXEC` alone, then repeat and assert the same.
- T4 (covers F8): thread A calls `pkey_free(0)` and asserts 0; thread B's next `pkey_alloc(0, 0)` then returns `EINVAL`, because the bitmap is shared. After `fork`, the child's `pkey_alloc` also returns `EINVAL`. After `execve`, the first `pkey_alloc` returns `ENOSPC` again.
- T5 (covers F3, F4): on a host whose `/proc/cpuinfo` lists `pku`, read PKRU with `RDPKRU` before and after the T2 calls and assert it is unchanged, and that a read and a write of an ordinary page still succeed, so no access rights were applied. On a host without PKU the test prints that it skipped this check (`RDPKRU` raises `#UD` there).
- T6 (covers F10, F1, F5): under `hermit run --verify` on ptrace, on any host, assert `pkey_alloc(0, 0)` returns `ENOSPC`, that `pkey_alloc(1, 0)` and `pkey_alloc(0, 0x4)` still return `EINVAL` (argument checks first), then that `pkey_free(0)` returns 0 and a following `pkey_alloc(0, 0)` returns `EINVAL`. Run the same program natively on a kernel booted with `nopku` once, to confirm the `EINVAL` after `pkey_free(0)`, which this body takes from kernel source and not from a run.
- U1 (covers F9; host-config): the CI host's CPU and kernel either support protection keys or do not, so CI sees only one of the two answers; the measurement host reports `pku` in `/proc/cpuinfo` and is built with `CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS=y`. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:821`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/322#issuecomment-5872911999
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/322: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pkey_alloc records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/322 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843; only the kernel can issue an MMU-enforced key and write PKRU); listed 8 flag behaviours and 5 tests; no test names pkey_alloc, so tracking_status stays todo; open question: virtualized CPUID hides PKU (detcore/src/cpuid.rs:66) while passthrough grants keys
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (CPU PKU and nopku, kernel build option CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS); F7 narrowed to exhausting the keys; new F9, the no-PKU ENOSPC and ENOSYS (host-config U1); no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 8 and 6 applied (emulate a CPU without PKU: EINVAL checks in mm/mprotect.c order, then ENOSPC; EINVAL after pkey_free(0)); target_policy passthrough->emulate; F2-F4 and F6-F8 narrowed to the no-PKU target, T2-T5 rewritten to match; new F10, T6; removed the CPUID-consistency question and the PKU-CI-host question, which the no-PKU tests no longer need

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated: protection-key calls now pass through a Detcore wrapper that records key use and names backend refusals; target stays emulate.

## Discussion

[Start a discussion about `pkey_alloc`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pkey_alloc&body=Discussion+for+the+%60pkey_alloc%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpkey_alloc.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pkey_alloc%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pkey_alloc" (https://github.com/rrnewton/hermit/issues/322), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/322.

### Open questions

- Not examined: whether a guest that revokes access to key 0 with `WRPKRU` can break Detcore's own syscall injection. That belongs with `pkey_mprotect` tracking.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pkey_alloc`, and by confirming hermit currently routes `pkey_alloc` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pkey_alloc(0, 0)                = 1
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/322#issuecomment-5872911999
