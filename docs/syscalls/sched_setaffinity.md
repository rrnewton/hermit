---
schema: syscall-pages/v1
syscall: sched_setaffinity
nr_x86_64: 203
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setaffinity&body=Discussion+for+the+%60sched_setaffinity%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setaffinity.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setaffinity%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`sched_setaffinity(pid, cpusetsize, mask)` restricts the set of CPUs a thread may run on. Detcore emulates it as a no-op (`detcore/src/syscalls/threads.rs:2114` at hermit f4055eb9cae4). It checks the size and the mask pointer, reads up to 16 bytes of the mask, and returns 0 without touching the host scheduler. Every guest thread stays on virtual CPU 0, so a later `sched_getaffinity` still reports CPU 0 only.

## Linux semantics

`pid` is a thread ID; 0 means the calling thread. The raw syscall returns 0 on success.

The kernel copies at most `cpumask_size()` bytes of `mask`. That size depends on the kernel configuration and the host's CPU count (`nr_cpu_ids`). A shorter `cpusetsize` leaves the missing high bits clear. Extra bytes past the kernel's size are ignored and never read.

The mask that takes effect is the request intersected with the thread's cgroup cpuset and the active CPUs. If nothing is left, the call fails with `EINVAL`. Other errors:

- `ESRCH`: no thread `pid` exists.
- `EPERM`: the caller lacks `CAP_SYS_NICE` and its effective UID matches neither the real nor the effective UID of the target.
- `EINVAL`: the target cannot be re-pinned, for example a per-CPU kernel thread, or a `SCHED_DEADLINE` thread whose root domain the mask does not cover.

On success the kernel may migrate the thread at once. A child created by `fork` or `clone` inherits the mask, and `execve` keeps it. The mask is visible through `sched_getaffinity`, through the `Cpus_allowed` lines of `/proc/<pid>/status`, and indirectly through `getcpu`.

Host probe on the 316-CPU agent host (Linux 7.1.3, python ctypes, 2026-10-03), recorded by this body's author:

- `cpusetsize` 0 gave `EINVAL`.
- An 8-byte all-zero mask gave `EINVAL`.
- pid 999999999 gave `ESRCH`.

### Flags and options

- F1: `pid` 0: change the calling thread's mask
- F2: `pid` naming another thread, in this or another process: change that thread's mask. `ESRCH` if no such thread exists; `EPERM` without permission.
- F3: `cpusetsize` shorter than the kernel cpumask: the missing high bits are treated as clear. `cpusetsize` 0 gives an empty mask and therefore `EINVAL`.
- F4: `cpusetsize` longer than the kernel cpumask: bytes past `cpumask_size()` are not read
- F5: a mask that selects at least one CPU the thread is allowed to use and that is online: returns 0, and from then on the thread runs only on those CPUs
- F6: a mask that selects no permitted online CPU (all zero, or only CPUs outside the cpuset or offline): `EINVAL`
- F7: `mask` NULL or unreadable: `EFAULT`
- F8: inheritance: children created by `fork`/`clone` inherit the mask, and `execve` keeps it

## Determinism analysis

`linux_determinism: conditional`. For one host and one launch configuration, a given request gets the same answer every time. Across hosts and launches the answer changes.

### Nondeterminism sources

- Host topology: the CPU count, the online CPUs (hotplug), and `cpumask_size()`. These decide whether a request for a fixed CPU number succeeds (F5) or gets `EINVAL` (F6), and how many bytes are read (F4).
- The cgroup cpuset and the affinity inherited from whatever launched the program (for example `taskset`). An outside process can change the cpuset while the guest runs. The header comment of `tests/c/fixture_cpuset_shim.c:12` records a real case: the dev-hermit validate launcher keeps cells off the CPUs the host PMU drivers use, CPU 0 among them (https://github.com/rrnewton/hermit/issues/3265). A fixture that asks for a fixed CPU therefore passes on one host and fails on another.
- `pid`: on the host it is a host thread ID. Whether it exists, and whose credentials it carries, depends on what else runs on the host.
- Placement: after a successful call, which CPU the thread actually runs on is up to the host scheduler. The guest can observe it through `getcpu`.

## Hermit design

Target policy: `emulate`. Detcore replaces the Linux scheduler with its own and runs every guest thread on one virtual CPU. The comment at `detcore/src/syscalls/threads.rs:78` says this "represents one virtual CPU in a fixed 128-bit kernel mask". A host affinity mask therefore has no guest-visible meaning under Hermit. Forwarding the call would leak host topology through F5 and F6.

What the handler does today (`handle_sched_setaffinity`, `detcore/src/syscalls/threads.rs:2114`, reached from the typed arm at `detcore/src/lib.rs:2636`):

- `cpusetsize` 0 returns `EINVAL` (`detcore/src/syscalls/threads.rs:2120`). This matches Linux, where an empty mask is `EINVAL` (F3).
- A NULL mask returns `EFAULT` (`detcore/src/syscalls/threads.rs:2124`).
- It reads `min(cpusetsize, 16)` bytes of the mask (`detcore/src/syscalls/threads.rs:2126`-`2130`), so an unreadable first part of the buffer also gives `EFAULT` (F7).
- It logs the request and returns 0 (`detcore/src/syscalls/threads.rs:2131`-`2135`). The kernel is never called, and `pid` is never read.
- The handler never injects a syscall, so record and replay run the same emulation. The syscall stays in the record/replay subscription (`detcore/src/lib.rs:1174`).

Related surfaces agree with the virtual CPU: `sched_getaffinity` always reports CPU 0 only (see that syscall's body), and `getcpu` reports CPU 0 (`detcore/src/syscalls/misc.rs:652`). `/proc/<pid>/status` has its `Cpus_allowed` and `Cpus_allowed_list` lines rewritten (`detcore/src/procfs.rs:1365`; unit test `status_normalizes_affinity_and_context_switches` at `detcore/src/procfs.rs:5360`). F8 therefore holds trivially: no mask is stored, so a child or an exec'd image sees the same CPU 0 answer.

Where the emulation differs from Linux. Each difference is deterministic, but a program can observe it:

- **F6 is never reported.** An all-zero mask, or a mask that names only CPU 1, returns 0. On a real one-CPU machine both would get `EINVAL`. The existing test asserts this on purpose: "even an empty requested set is normalized to virtual CPU 0" (`detcore/tests/misc/mod.rs:1022`). Under the alternative, a mask without CPU 0 would get `EINVAL`, matching the one-CPU machine the other surfaces describe. The current rule is more permissive: a program that pins to a CPU number it learned elsewhere keeps running. Owner decision 6 chose `EINVAL`; see the Target paragraph below.
- **F2 ignores `pid`.** An unknown thread ID returns 0 instead of `ESRCH`, and `EPERM` never happens. A modeled version would look the guest's virtual TID up in Detcore's thread table and return `ESRCH` when it is absent. The host is not consulted, so the answer stays deterministic.
- **F4 reads only 16 bytes.** Linux reads `cpumask_size()` bytes (40 on the 316-CPU agent host), so a buffer that becomes unreadable after 16 bytes gives `EFAULT` there and 0 under Hermit.

`emulate` fits better than the alternatives:

- `model` would need per-thread mask state, which would only matter if Hermit ever exposed more than one virtual CPU.
- `refuse` would break every runtime that pins threads at startup (thread pools, `taskset`, numactl).
- `passthrough` leaks host topology.

Target (owner decisions 6 and 7): `target_policy` stays `emulate`. Decision 7 asks that `sched_getaffinity` report what `sched_setaffinity` set. On Linux the stored mask is the request intersected with the CPUs the thread may use (kernel `__sched_setaffinity()`), and the reported mask is the stored one intersected with the active CPUs (kernel `sched_getaffinity()`). On the one-CPU virtual machine every accepted request therefore reads back as CPU 0 alone, which `handle_sched_getaffinity` already writes (`detcore/src/syscalls/threads.rs:2156`-`2157`), so no per-thread mask is needed while Hermit exposes one virtual CPU. A per-thread mask, inherited at fork and kept across `execve` (F8), becomes necessary only if Hermit exposes more virtual CPUs. Decision 6 brings the kernel's check order (sched_setaffinity(2); kernel `get_user_cpu_mask()`, then `sched_setaffinity()`):
  1. Copy `min(cpusetsize, 16)` bytes, 16 being the virtual `cpumask_size()`. A failed copy is `EFAULT`, mapping the backend's `EIO` as `handle_sched_setattr` does (`detcore/src/syscalls/threads.rs:2336`). `cpusetsize` 0 copies nothing, so a NULL mask with size 0 is not `EFAULT` (F3, F4, F7).
  2. `ESRCH` when `pid` names no live guest thread, from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`); any thread of any guest process is accepted (F1, F2).
  3. `EINVAL` when the copied mask does not include CPU 0, which covers an all-zero mask and `cpusetsize` 0 (F3, F6).
  4. Return 0 with no effect on the host or on Detcore's scheduler (F5).

`EPERM` (F2) stays unreported, under the privilege rule at `detcore/src/syscalls/threads.rs:2294`. This replaces the empty-mask normalization: the assertions at `detcore/tests/misc/mod.rs:973`-`983` (a mask naming only CPU 1 returns 0) and `detcore/tests/misc/mod.rs:1012`-`1022` (an all-zero mask returns 0) change to expect `EINVAL`. Today `cpusetsize` 0 is refused before any lookup (`detcore/src/syscalls/threads.rs:2120`), a NULL mask is refused before the copy (`detcore/src/syscalls/threads.rs:2124`), and `pid` is never read.

## Recommended tests

- T1 (covers F1, F3, F5, F7): on the calling thread, check that a mask selecting CPUs 0 and 1 returns 0, `cpusetsize` 0 gives `EINVAL`, and a NULL mask gives `EFAULT`. Afterwards `sched_getaffinity` must still report CPU 0 only.
  Existing candidate: `sched_affinity_is_normalized_to_virtual_cpu_zero` at `detcore/tests/misc/mod.rs:885` (test target `tests_misc`, run through `det_test_fn_sequential_without_pmu`). It asserts each of these at `detcore/tests/misc/mod.rs:973`-`1023`, but with a CPU-1-only mask returning 0, which changes to `EINVAL` under the target (T4).
- T2 (covers F2): pass the TID of another guest thread, the PID of another guest process, a TID that does not exist, and a negative pid. Today every case returns 0. Under the target the unknown and negative cases give `ESRCH`, and the other thread and the other process return 0.
- T3 (covers F3, F4): try `cpusetsize` 1, 8, 16, 128 and 1024 with a readable buffer of that size. Also try `cpusetsize` 1024 with a 16-byte mask placed at the end of a mapped page, followed by an unmapped page. Pin the results under `hermit run --verify`.
- T4 (covers F6): an all-zero mask and a mask naming only CPU 5. Today both return 0 (`detcore/tests/misc/mod.rs:1022` covers the all-zero case). Under the target (owner decision 6) both give `EINVAL`, as on a one-CPU Linux machine.
- T5 (covers F8): call `sched_setaffinity` asking for CPU 1, then fork a child that execs a program calling `sched_getaffinity` and `getcpu` and reading `Cpus_allowed_list` from `/proc/self/status`. Under `hermit run --verify` all three must report CPU 0 only.
- T6 (covers F5, F6): host independence. Run a guest that asks for CPU 0, prints the return value, then prints the `sched_getaffinity` mask. Run it under `hermit run --verify` twice, once directly and once under a `taskset -c 3` launcher, and require byte-identical output. Natively the printed mask differs between the two launches. (A process may widen its own mask to any CPU in its cpuset, as `tests/c/fixture_cpuset_shim.c:20`-`22` notes, so the request for CPU 0 itself succeeds natively in both launches; only a real cpuset makes it fail.)
- T7 (covers F2, F3, F7): check order. `cpusetsize` 0 with a NULL mask gives `EINVAL`, not `EFAULT`; `cpusetsize` 0 with a TID that does not exist gives `ESRCH`; an unmapped mask with a TID that does not exist gives `EFAULT`; a mask naming only CPU 5 with a TID that does not exist gives `ESRCH`. Today the first two give `EINVAL` and the last returns 0.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy emulate (accept-and-discard on one virtual CPU; threads.rs:2114 returns 0 after the size and pointer checks without calling the kernel); linux_determinism stays conditional (host topology, cpuset and launcher affinity decide F5/F6); 8 flags and 6 tests listed; tracking_status stays todo: no coverage recorded, and the F6 normalization and ignored pid (F2) are open questions for the owner
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 7 applied (kernel order EFAULT copy, ESRCH from Detcore's task table, then EINVAL for a mask without CPU 0, replacing the empty-mask normalization; decision 7's read-back already holds on one virtual CPU, so no per-thread mask yet); target_policy stays emulate; new T7; T1, T2 and T4 expectations updated to the target; removed both open questions (EINVAL for a mask without CPU 0; pid ESRCH)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_setaffinity`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setaffinity&body=Discussion+for+the+%60sched_setaffinity%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setaffinity.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setaffinity%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sched_setaffinity`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The handler carries `TODO-HUMAN-REVIEW(#546)`. That tag names https://github.com/rrnewton/hermit/pull/546 ("Determinize scheduler affinity masks", commit f6f4d78992, 2026-07-24), which added both affinity handlers.

`tests/c/fixture_cpuset_shim.c` names this syscall, but it is not a Hermit test. It is a link-time wrapper the fixture-mutation harness uses to simulate a host cpuset (`tests/c/fixture_cpuset_shim.c:23`-`28`).
