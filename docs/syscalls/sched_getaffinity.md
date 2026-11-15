---
schema: syscall-pages/v1
syscall: sched_getaffinity
nr_x86_64: 204
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getaffinity&body=Discussion+for+the+%60sched_getaffinity%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getaffinity.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getaffinity%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 7
---

## Summary

`sched_getaffinity(pid, cpusetsize, mask)` reports the set of CPUs a thread may run on. Detcore emulates it with a fixed answer (`detcore/src/syscalls/threads.rs:2141` at hermit f4055eb9cae4). The answer is a 16-byte mask with only CPU 0 set, and the return value is 16, whatever the host has and whatever `sched_setaffinity` asked for. The kernel is never consulted.

## Linux semantics

`pid` is a thread ID; 0 means the calling thread. The kernel checks the buffer size first. It fails with `EINVAL` if `cpusetsize * 8` is smaller than `nr_cpu_ids`, or if `cpusetsize` is not a multiple of `sizeof(unsigned long)`.

The kernel then reports the thread's allowed CPUs intersected with the active CPUs. It copies `min(cpusetsize, cpumask_size())` bytes and returns that count. Bytes past the count are left untouched. The glibc wrapper returns 0 instead, and zero-fills the rest of the caller's buffer.

Other errors:

- `ESRCH`: no thread `pid` exists, including any negative `pid`.
- `EFAULT`: `mask` cannot be written. This is checked after the size checks.

The reported mask reflects earlier `sched_setaffinity` calls, the mask inherited from the parent and the launcher, and the cgroup cpuset. Tools such as coreutils `nproc` and many thread-pool runtimes size themselves from it.

Host probe on the 316-CPU agent host (Linux 7.1.3, python ctypes, 2026-10-03), recorded by this body's author:

- `cpusetsize` 8, 12 and 16 gave `EINVAL`.
- 40, 64, 128 and 1024 each returned 40, which is `cpumask_size()` for `nr_cpu_ids` = 316.
- pid -5 gave `ESRCH`.

### Flags and options

- F1: `pid` 0: report the calling thread's mask
- F2: `pid` naming another thread: report that thread's mask. `ESRCH` if no such thread exists, and for any negative pid.
- F3: `cpusetsize` too small (`cpusetsize * 8 < nr_cpu_ids`): `EINVAL`
- F4: `cpusetsize` not a multiple of `sizeof(unsigned long)` (8 on x86_64): `EINVAL`
- F5: `cpusetsize` larger than the kernel cpumask: only `cpumask_size()` bytes are written, and that count is returned. Later bytes are untouched.
- F6: `mask` NULL or unwritable, with a valid size: `EFAULT`
- F7: reported content: the thread's allowed CPUs intersected with the active CPUs. This reflects earlier `sched_setaffinity`, the inherited mask and the cpuset.

## Determinism analysis

`linux_determinism: conditional`. On one host, with one launch configuration and no outside changes, the call repeats. Across hosts or launches both the return value and the mask change, and so does the size at which the call starts to fail.

### Nondeterminism sources

- Host topology: `nr_cpu_ids` sets the smallest buffer that is accepted (F3) and the return value (F5). The probe above shows a 16-byte buffer rejected on a 316-CPU host; on a host with 64 CPUs or fewer it is accepted.
- Online and active CPUs, which change with CPU hotplug
- The cgroup cpuset and the inherited affinity. Both are set by whatever launched the program (a `taskset`, a container runtime, the validate launcher of https://github.com/rrnewton/hermit/issues/3265), and an outside process can change the cpuset while the guest runs.
- `pid` on the host is a host thread ID, so whether it exists depends on other processes

A program that sizes a thread pool from this mask does different work on different hosts. That is why Hermit must not pass it through.

## Hermit design

Target policy: `emulate`. Detcore runs every guest thread on one virtual CPU and presents that topology consistently. The comment at `detcore/src/syscalls/threads.rs:78` says the constant `VIRTUAL_CPUSET_BYTES` = 16 (`detcore/src/syscalls/threads.rs:80`) preserves "the historical Detcore ABI while hiding the host's configured CPU count".

What the handler does today (`handle_sched_getaffinity`, `detcore/src/syscalls/threads.rs:2141`, reached from the typed arm at `detcore/src/lib.rs:2635`):

- It returns `EINVAL` when `cpusetsize` is below 16 or not a multiple of 8 (`detcore/src/syscalls/threads.rs:2147`-`2150`). These are F3 and F4 for a fixed virtual `nr_cpu_ids` of 128.
- It builds a 16-byte mask with only bit 0 set (`detcore/src/syscalls/threads.rs:2156`-`2157`), writes exactly those 16 bytes (`detcore/src/syscalls/threads.rs:2165`), and returns 16 (`detcore/src/syscalls/threads.rs:2170`). Bytes past 16 are untouched, as in F5.
- A NULL mask returns `EFAULT` (`detcore/src/syscalls/threads.rs:2172`), after the size checks, as in F6. An unwritable non-NULL mask fails the write with `EFAULT`.
- `pid` is never read, and the handler never injects a syscall. Record and replay therefore run the same emulation. The syscall stays in the record/replay subscription (`detcore/src/lib.rs:1173`).

Other surfaces present the same one-CPU topology:

- `getcpu` reports CPU 0 (`detcore/src/syscalls/misc.rs:652`).
- The `Cpus_allowed` and `Cpus_allowed_list` lines of `/proc/<pid>/status` are rewritten (`detcore/src/procfs.rs:1365`; unit test `status_normalizes_affinity_and_context_switches` at `detcore/src/procfs.rs:5360`).
- `sched_setaffinity` is accepted and discarded, so F7 always reports CPU 0 (see that syscall's body).

Where the emulation differs from Linux. Each difference is deterministic:

- **F2 ignores `pid`.** A thread that does not exist, or a negative pid, gets the CPU 0 answer instead of `ESRCH`. A modeled version would look the guest's virtual TID up in Detcore's thread table.
- **F3 uses a fixed threshold of 16 bytes.** An 8-byte buffer, which a host with 64 CPUs or fewer accepts, gets `EINVAL` under Hermit. One way to hit this is `CPU_ALLOC(1)` (8 bytes). This is a hypothesis from the code, not a measured failure. Callers that retry with a larger buffer on `EINVAL` recover; coreutils `nproc` is believed to do this. T5 measures it.

`emulate` fits better than the alternatives:

- `passthrough` leaks host topology.
- `refuse` breaks every runtime that sizes itself from the mask.
- `model` would add per-thread state that has no use while Hermit exposes a single virtual CPU.

Target (owner decisions 6 and 7): `target_policy` stays `emulate`. Decision 7's read-back holds without per-thread state: on the one-CPU virtual machine every mask that `sched_setaffinity` accepts (where decision 6 makes a mask without CPU 0 `EINVAL`) intersects the active CPUs to CPU 0 alone, and that is what this handler reports, as Linux reports the stored mask intersected with the active CPUs (F7; kernel `sched_getaffinity()`). Decision 6 brings the kernel's order (sched_getaffinity(2); kernel `sys_sched_getaffinity()`): first the size checks, `EINVAL` (F3, F4), against the virtual 16-byte cpumask as today (`detcore/src/syscalls/threads.rs:2147`-`2150`); then `ESRCH` when `pid` names no live guest thread, negative pids included, from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`) (F2); then copy `min(cpusetsize, 16)` bytes, 16 in practice, and return 16, where a NULL or unwritable mask is `EFAULT` (F5, F6). Today `pid` is never read, and a NULL mask is `EFAULT` (`detcore/src/syscalls/threads.rs:2172`) even when `pid` names no thread, where Linux gives `ESRCH`.

## Recommended tests

- T1 (covers F1, F3, F4, F5, F6, F7): call on the calling thread with a 32-byte buffer filled with a sentinel. The call must return 16, set only bit 0, and leave bytes 16-31 untouched. 15 and 17 bytes must give `EINVAL`, a NULL mask with 15 bytes must give `EINVAL`, and a NULL mask with 16 bytes must give `EFAULT`. After `sched_setaffinity`, the mask must still be CPU 0.
  Existing candidate: `sched_affinity_is_normalized_to_virtual_cpu_zero` at `detcore/tests/misc/mod.rs:885` (test target `tests_misc`). It asserts the sentinel and untouched-tail case at `detcore/tests/misc/mod.rs:898`-`906`.
- T2 (covers F1, F5, F7): repeated raw calls under `hermit run --verify` on ptrace must return 16 and a byte-identical mask, with only CPU 0 set.
  Existing candidates: `tests/c/sched_getaffinity_identity.c` (cell `c-programs/sched-getaffinity-identity`, `tests/e2e/manifests/c-programs.yaml:15784`, verify ci on ptrace), and `tests/c/cpu_virtualization.c:44` (cell `c-programs/cpu-virtualization`, `tests/e2e/manifests/c-programs.yaml:15642`, verify ci on ptrace), which uses a 128-byte `cpu_set_t` and checks `CPU_COUNT` = 1.
- T3 (covers F2): pass the TID of another guest thread, the PID of another guest process, a TID that does not exist, and pid -5. Today every case reports CPU 0. Under the target the last two cases give `ESRCH`, and the first two report CPU 0.
- T4 (covers F7): host independence. Run the T2 fixture directly and under a `taskset -c 3` launcher, and require byte-identical output under `--verify`. Natively the two masks differ.
- T5 (covers F3, F5): an 8-byte buffer (`CPU_ALLOC(1)`) gives `EINVAL`, and buffers of 128, 1024 and 0x8000 bytes return 16. Also run coreutils `nproc` under Hermit and check that it prints 1 and exits 0.
  Related existing test: `detcore/tests/lit/sched_getaffinity/main.rs` calls the nix wrapper and raw buffers from 128 to 0x8000 bytes, and asserts only that some bit is set. This body did not check which runner executes the lit tests.
- T6 (covers F2, F3, F6): check order. A NULL mask with `cpusetsize` 16 and a TID that does not exist gives `ESRCH`; the same with `pid` 0 gives `EFAULT`; `cpusetsize` 15 with a TID that does not exist gives `EINVAL`. Today the first gives `EFAULT`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy emulate (fixed 16-byte CPU-0-only mask, return 16, pid ignored; threads.rs:2141); linux_determinism stays conditional (nr_cpu_ids, hotplug, cpuset and launcher affinity); 7 flags and 5 tests listed, with existing candidates in detcore/tests/misc/mod.rs:885 and two verify-on-ptrace manifest cells; tracking_status stays todo: no coverage recorded yet, and whether to check pid (F2) is an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 7 applied (kernel order: size EINVAL, then ESRCH from Detcore's task table, then the copy-out EFAULT; decision 7's read-back already holds on one virtual CPU); target_policy stays emulate; new T6; T3 expectation updated to the target; removed the pid ESRCH question; kept the 16-byte minimum question (a virtual-topology choice no decision answers)
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/sched_getaffinity_identity.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_getaffinity`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getaffinity&body=Discussion+for+the+%60sched_getaffinity%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getaffinity.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getaffinity%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sched_getaffinity`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The handler carries `TODO-HUMAN-REVIEW(#546)`. That tag names https://github.com/rrnewton/hermit/pull/546 ("Determinize scheduler affinity masks", commit f6f4d78992, 2026-07-24).

`tests/c/fixture_cpuset_shim.c` names this syscall only in its constructor, which reads the startup mask to simulate a cpuset (`tests/c/fixture_cpuset_shim.c:59`-`64`). It is harness code, not a Hermit test.

### Open questions

- Is the fixed 16-byte minimum (F3) acceptable? A one-CPU Linux machine accepts 8 bytes. T5 should show whether real callers that start from `CPU_ALLOC(1)` recover.
