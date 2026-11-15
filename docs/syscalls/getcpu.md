---
schema: syscall-pages/v1
syscall: getcpu
nr_x86_64: 309
linux_determinism: nondeterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getcpu&body=Discussion+for+the+%60getcpu%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetcpu.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getcpu%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`getcpu(cpu, node, tcache)` reports which CPU the calling thread is running on and which NUMA node that CPU belongs to. Detcore handles it (`detcore/src/lib.rs:2749`, `detcore/src/syscalls/misc.rs:651`). It presents a single virtual CPU 0 on node 0: it writes 0 through each non-NULL pointer and returns 0, so the host CPU never reaches the guest through this syscall.

Manual reference: Linux man-pages getcpu(2), sched_getcpu(3).

## Linux semantics

The kernel reads the CPU the thread is running on at the moment of the call (`raw_smp_processor_id()`) and maps it to its node with `cpu_to_node()`. The answer can be out of date by the time the caller reads it, because the scheduler may migrate the thread right after the call.

Each output pointer is optional. When both are given, the kernel attempts both writes and returns `EFAULT` if either fails, so a bad `cpu` pointer does not stop the `node` write. The third argument, `tcache`, has been unused since Linux 2.6.24 and is ignored.

Most programs never make the syscall. glibc's `sched_getcpu()` first reads the `cpu_id` field of the thread's registered rseq area (glibc 2.35 and later), and otherwise calls the vDSO entry point `__vdso_getcpu`, which answers in user space from `RDPID`/`RDTSCP` or the per-CPU segment limit. Only when neither is available does it issue `SYS_getcpu`.

### Flags and options

- F1: `cpu` non-NULL: the current CPU number is stored in `*cpu`.
- F2: `node` non-NULL: the NUMA node of the current CPU is stored in `*node`.
- F3: `cpu` and/or `node` NULL: that output is skipped. Both NULL returns 0 and writes nothing.
- F4: `tcache`: ignored, whatever its value, including an invalid pointer.
- F5: an output pointer that cannot be written: `EFAULT`. Linux attempts both writes before it returns, so the other output is still stored.
- F6: the vDSO entry point `__vdso_getcpu`, which answers without entering the kernel.
- F7: glibc `sched_getcpu()` reading the kernel-maintained rseq `cpu_id` field, which also answers without entering the kernel.

## Determinism analysis

`linux_determinism: nondeterministic`. The answer is whatever CPU the host scheduler placed the thread on at that instant, and the node follows from the CPU and the host topology. Both vary from run to run and from host to host. The call repeats only when the thread is pinned to a single CPU, and even then the number reflects the host's CPU numbering.

### Nondeterminism sources

- Host scheduler placement and migration of the calling thread
- The host's CPU and NUMA topology, which decides the node number and the range of CPU numbers
- User-space paths (F6, F7) that read the same host value without a syscall, so an interceptor that sees only syscalls misses them

## Hermit design

Target policy: `emulate`. Detcore's scheduler presents the guest with a single virtual CPU, so the consistent answer is CPU 0 on node 0, fixed for every thread and every run. `handle_getcpu` (`detcore/src/syscalls/misc.rs:651-668`) writes 0 through `cpu` if it is non-NULL, then 0 through `node` if it is non-NULL, and returns 0. It never reads the host value, and it ignores `tcache` (F1-F4).

Why `emulate` and not the alternatives:

- `passthrough` would hand the guest the host CPU number, which is the nondeterminism.
- `model` would mean tracking a per-thread virtual CPU. Detcore runs guest threads one at a time on one virtual CPU, so there is no state that could differ between threads, and the fixed answer is that model.
- `refuse` would break `sched_getcpu()` users, such as per-CPU allocators and thread pools, for no benefit.

The answer agrees with `sched_getaffinity`, which Detcore also emulates: it reports a mask containing only CPU 0 (`detcore/src/syscalls/threads.rs:2156-2157`).

What remains before the code fully realizes `emulate`:

- **Fault ordering (F5).** The handler propagates the first failed write with `?` (`detcore/src/syscalls/misc.rs:658-665`), so a bad `cpu` pointer leaves `*node` unwritten. Linux writes `*node` anyway. The return value, `EFAULT`, is the same.
- **vDSO entry point (F6).** On ptrace, the pinned Reverie submodule (d766df20, `reverie-ptrace/src/vdso.rs:59-65` and `:277`) replaces `__vdso_getcpu` with a stub that issues the real syscall whenever the tool subscribes to `getcpu` (`reverie-ptrace/src/vdso.rs:358-366`), so the vDSO path reaches `handle_getcpu`. The comment on the `c-programs/getcpu-identity` cell says the vDSO path "does not route through syscall interception"; at this SHA that reads as stale for ptrace. T4 confirms it.
- **rseq `cpu_id` (F7).** Outside fail-closed mode, Detcore passes `rseq` through to the kernel (`detcore/src/lib.rs:2029-2037`). If the kernel registers glibc's rseq area, it keeps `cpu_id` at the host CPU, and `sched_getcpu()` can read the host value without any syscall. This is a hypothesis from the code, not a measurement: no test at this SHA calls `sched_getcpu()`. T5 measures it. Under fail-closed mode `rseq` returns `ENOSYS`, glibc falls back to the vDSO path, and the leak cannot happen.

`linux_determinism` stays `nondeterministic`: the host answer varies, and Hermit replaces it with a constant.

## Recommended tests

- T1 (covers F1, F2, F3): with a raw `syscall(SYS_getcpu, ...)` and poisoned outputs, call with both pointers, `cpu` only, `node` only and both NULL, several times each, and check that every written value is 0 and every return is 0, identically across `hermit run --verify` runs on ptrace.
  Existing candidates: `tests/c/getCpu.c` (cell `c-programs/getcpu`, `tests/e2e/manifests/c-programs.yaml:1921`) and `tests/c/getcpu_identity.c` (cell `c-programs/getcpu-identity`, `tests/e2e/manifests/c-programs.yaml:15713`), both verify cells with ptrace enabled. Neither calls with both pointers NULL.
- T2 (covers F4): pass `tcache` as NULL, as a valid pointer and as an unmapped address such as `(void *)1`. The call must succeed each time and never write through `tcache`.
- T3 (covers F5): pass an unmapped `cpu` with a valid `node`, and the reverse. Each call must return `EFAULT`. Check whether `*node` was written in the first case: Linux writes it, and the handler at this SHA does not. Record the difference as a defect if it is confirmed.
- T4 (covers F6): resolve `__vdso_getcpu` from the auxiliary vector and call it directly under Hermit on ptrace. It must report CPU 0, node 0, and must match the raw syscall.
  Related existing coverage: `tests/c/cpu_virtualization.c` (cell `c-programs/cpu-virtualization`, `tests/e2e/manifests/c-programs.yaml:15642`) checks the raw syscall at `tests/c/cpu_virtualization.c:27-36`, not the vDSO path.
- T5 (covers F7): call glibc `sched_getcpu()` repeatedly from several threads under `hermit run --verify` on ptrace, without fail-closed mode. It must report 0 every time. A nonzero value would confirm the rseq `cpu_id` leak described under Hermit design.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy emulate (fixed virtual CPU 0 on node 0, misc.rs:651); linux_determinism nondeterministic (host scheduler placement); 7 flag behaviours and 5 tests listed; open questions on the rseq cpu_id path for sched_getcpu() and the EFAULT write order; no coverage token recorded, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getcpu`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getcpu&body=Discussion+for+the+%60getcpu%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetcpu.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getcpu%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getcpu`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Does glibc's `sched_getcpu()` see the host CPU through the rseq `cpu_id` field when Detcore passes `rseq` through (`detcore/src/lib.rs:2029-2037`)? If it does, the fix belongs with `rseq` (refuse registration, or virtualize `cpu_id`), not here. T5 decides it.
- Should `handle_getcpu` attempt both writes before it returns `EFAULT`, to match Linux exactly (F5)? The guest-visible difference is limited to `*node` after a bad `cpu` pointer.
- The comment on the cell `c-programs/getcpu-identity` says glibc's vDSO `sched_getcpu` path is not intercepted. The pinned Reverie code rewrites `__vdso_getcpu` into a syscall on ptrace, so the comment appears stale for that backend.
