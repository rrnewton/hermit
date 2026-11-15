---
schema: syscall-pages/v1
syscall: setrlimit
nr_x86_64: 160
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/457
  - https://github.com/rrnewton/hermit/issues/465
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setrlimit&body=Discussion+for+the+%60setrlimit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetrlimit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setrlimit%60%2C+or+in+what+the+page+says%3F%0A
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

`setrlimit(resource, rlim)` replaces the soft and hard limit of one process resource. Detcore emulates it (`detcore/src/syscalls/sysinfo.rs:188`) against a per-process virtual limit table with fixed initial values. It never forwards the call to the kernel, so the host's limits stay as they were. It accepts real changes only for `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_CORE`, and only changes that do not raise the hard limit. Every other real change gets `EPERM`, and an exact no-op succeeds for every resource.

Manual reference: [setrlimit(2)](https://man7.org/linux/man-pages/man2/getrlimit.2.html).

## Linux semantics

`setrlimit` acts on the calling process. Limits are process state: all threads share one set, `fork` copies it, and `execve` keeps it. The kernel entry first copies the `struct rlimit` from user memory, which gives `EFAULT` if that fails. Then `do_prlimit` checks:

1. The resource number must be below `RLIM_NLIMITS` (16 on x86_64); otherwise `EINVAL`.
2. `rlim_cur` must not exceed `rlim_max`; otherwise `EINVAL`.
3. For `RLIMIT_NOFILE`, `rlim_max` must not exceed `/proc/sys/fs/nr_open`; otherwise `EPERM`, even for a privileged caller.
4. Raising `rlim_max` above its current value needs `CAP_SYS_RESOURCE`; otherwise `EPERM`. This is `capable()`, so it checks the initial user namespace: root inside a user namespace cannot raise a hard limit either.
5. A Linux security module can still refuse the change.

Without privilege, a process can lower either limit and raise its soft limit up to the hard limit. Setting the current pair again is accepted for every resource; both host probes under Additional information did this, reapplying the current `RLIMIT_NOFILE` pair and the current `RLIMIT_CORE` pair, and both returned 0. A lowered hard limit cannot be raised again without privilege. `RLIM_INFINITY` means no limit.

Modern glibc builds `getrlimit(3)` and `setrlimit(3)` on top of `prlimit64(2)`, so only programs that issue the raw syscall reach this entry point. `tests/c/rlimit_identity.c:66-84` documents this, together with the measurement that a deliberate divergence planted in the getrlimit handler went unseen through the library wrappers. The legacy text called `setrlimit` superseded by `prlimit64` for the same reason.

Changing a limit changes what later system calls do: `EMFILE`, `EFBIG` with `SIGXFSZ`, `SIGXCPU`, `ENOMEM`, `EAGAIN` from `fork`, the size of core dumps, and stack growth.

### Flags and options

- F1: `RLIMIT_CPU`: process CPU time in seconds. When the soft limit is reached the kernel sends `SIGXCPU`, and `SIGKILL` at the hard limit. Changing it re-arms the process CPU timer.
- F2: `RLIMIT_FSIZE`: largest file the process may create or extend. A write past it fails with `EFBIG` and raises `SIGXFSZ`.
- F3: `RLIMIT_DATA`: size of the data segment, heap and private writable mappings. `brk` and `mmap` past it fail with `ENOMEM`.
- F4: `RLIMIT_STACK`: largest main-thread stack. It bounds stack growth, and at `execve` it also selects the memory map layout and the room left for arguments.
- F5: `RLIMIT_CORE`: largest core dump; 0 disables core dumps.
- F6: `RLIMIT_RSS`: resident set size. It is accepted and stored, but the kernel no longer enforces it.
- F7: `RLIMIT_NPROC`: number of processes for the real user ID. `fork` and `clone` past it fail with `EAGAIN`.
- F8: `RLIMIT_NOFILE`: one more than the highest descriptor number that may be allocated. `open`, `dup` and others past it fail with `EMFILE`.
- F9: `RLIMIT_MEMLOCK`: bytes of memory that may be locked with `mlock` and similar calls.
- F10: `RLIMIT_AS`: size of the virtual address space. `mmap`, `brk` and stack growth past it fail.
- F11: `RLIMIT_LOCKS`: count of `flock` and `fcntl` locks. Stored, but unused since Linux 2.4.25.
- F12: `RLIMIT_SIGPENDING`: queued signals for the real user ID. A real-time signal sent past it fails with `EAGAIN`.
- F13: `RLIMIT_MSGQUEUE`: bytes of POSIX message queues for the real user ID.
- F14: `RLIMIT_NICE`: ceiling for raising the nice value, as `20 - rlim_cur`.
- F15: `RLIMIT_RTPRIO`: ceiling for real-time priority without privilege.
- F16: `RLIMIT_RTTIME`: microseconds of CPU a real-time thread may use without blocking, enforced with `SIGXCPU` and then `SIGKILL`.
- F17: `rlim_cur > rlim_max`: `EINVAL`, checked before any privilege rule.
- F18: raising `rlim_max` above its current value: `EPERM` without `CAP_SYS_RESOURCE` in the initial user namespace.
- F19: `RLIMIT_NOFILE` with `rlim_max` above `/proc/sys/fs/nr_open` (default 1048576): `EPERM`, whatever the caller's privilege.
- F20: a resource number that is not valid (`>= RLIM_NLIMITS`, or negative, which the kernel reads as a large unsigned value): `EINVAL`.
- F21: `rlim` is `NULL` or points to unreadable memory: `EFAULT`. The kernel copies the struct before it looks at the resource number, so `EFAULT` wins over `EINVAL`.
- F22: changes that need no privilege: setting the current pair again (a no-op, accepted for every resource), lowering the soft or the hard limit, and raising the soft limit up to the hard limit.
- F23: `RLIM_INFINITY` as either value: no limit. It compares as the largest value, so a finite hard limit with an infinite soft limit is `EINVAL`.
- F24: scope: the change is visible to every thread of the process, is copied to children at `fork`, and survives `execve`.
- F25: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). When the host's real limit is below the pair the guest set or was given, a failure it causes in a later call (for example `EMFILE` from `openat` while the guest's soft `RLIMIT_NOFILE` is higher) is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>`, not handed to the guest.

## Determinism analysis

`linux_determinism: conditional`, kept from both legacy classifications. On a given set of arguments the result depends on three things the program does not choose: the limits it inherited from the host, its capabilities, and the `nr_open` sysctl. The call repeats when all three are the same. The change then decides later results: whether an `open` gets `EMFILE`, whether a write raises `SIGXFSZ`, and when `SIGXCPU` arrives. Those depend on how many resources the process holds, which is often not deterministic either. Because the initial limits are host policy, Hermit has to either virtualize the limits it supports or reject changes deterministically; it does both (Hermit design).

### Nondeterminism sources

- Inherited host limits. The host strace lines under Additional information show the probe host's `RLIMIT_NOFILE` at 512*1024 and `RLIMIT_CORE` at infinity. `tests/c/rlimit_identity.c:38-40` records the same host reporting a native hard `RLIMIT_NOFILE` of 524288.
- Capabilities (`CAP_SYS_RESOURCE`) and user namespace membership, which decide whether a hard limit can be raised.
- The `/proc/sys/fs/nr_open` sysctl, which bounds `RLIMIT_NOFILE`.
- Current resource consumption and kernel accounting, which decide when an enforced limit is reached.
- Security modules that can refuse a change.

## Hermit design

Target policy: `model` (changed from `emulate`, owner decision 5, 2026-10-04: limits become real and deterministic). Detcore keeps deciding the result from the virtual table, so the call's own answer never depends on the host. The target adds that every limit the guest has set or been given is the limit in force: a later call fails at exactly the soft limit `getrlimit`/`prlimit64` report. Hermit gets there per resource, in one of two ways: Detcore enforces the limit itself (for `RLIMIT_NOFILE`, by counting the guest's descriptor table), or Hermit configures the guest's host rlimit to the reported value at launch and on each accepted `setrlimit`, so the two cannot disagree. This body does not choose between them. Once a resource is enforced or configured, there is no reason left to refuse the changes Linux allows without privilege (F22), lowering included; until then, refusing a real change with `EPERM` stays the honest answer, because an accepted change would promise a guard that never fires.

Interim, until a resource reaches the target (F25): a failure caused by a host-enforced limit that the reported limit would not predict is a determinism failure, not a guest failure. Detcore reports it as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>` through the infrastructure-error convention instead of passing it through. Today it is passed through silently; for example the error arm of `handle_openat` hands the host's `EMFILE` to the guest unchanged (`detcore/src/syscalls/files.rs:1130`-`1132`).

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): this replaces the interim refusal above. The starting values are fixed with soft equal to hard for every resource (the table is in the `getrlimit` body: `RLIMIT_STACK` 8 MiB, `RLIMIT_NOFILE` 1048576, `RLIMIT_MEMLOCK` 0, every other resource `RLIM_INFINITY`). A lowering of either limit is accepted for all 16 resources, forwarded to the guest's host limit and recorded in the virtual table, so the lowered limit is in force through the host kernel and `getrlimit`/`prlimit64` read it back. A raise of the hard limit is `EPERM` (F18), as Linux gives without `CAP_SYS_RESOURCE`. A soft raise above the hard limit stays `EINVAL` (F17). After a guest lowers only its soft limit, raising it back up to the hard limit is accepted and forwarded (F22): Linux allows it without privilege, the host accepts the same raise, and the approval's "raising returns `EPERM`" rests on the hard-limit rule. This lifts the restriction at `detcore/src/syscalls/sysinfo.rs:147`-`152`, so `ulimit -v`, `ulimit -t` and `ulimit -f` work in guest shells. Any host-enforced surprise that remains is the `cause=host-rlimit` infrastructure error (F25).

Today the arm `Syscall::Setrlimit(s) => self.handle_setrlimit(guest, s)` (`detcore/src/lib.rs:2781`) never reaches the kernel. Code at hermit f4055eb9cae4:

- The limits live in a `ResourceLimits` table on the thread state, one entry per resource from 0 to `RLIMIT_RTTIME` (`detcore/src/tool_local.rs:274-295`). Its initial values are fixed: every resource is `RLIM64_INFINITY`/`RLIM64_INFINITY`, except `RLIMIT_STACK` at 8 MiB soft and infinite hard (`detcore/src/tool_local.rs:285-288`) and `RLIMIT_NOFILE` at 1048576/1048576 (`detcore/src/tool_local.rs:289-292`). The unit test `defaults_are_fixed_and_cover_linux_resources` (`detcore/src/tool_local.rs:951`) pins these values.
- Threads share one table and a forked process gets a copy (`detcore/src/lib.rs:1566-1578`), as F24 requires. The unit test `cloned_process_state_changes_independently` (`detcore/src/tool_local.rs:978`) checks the copy.
- The handler (`detcore/src/syscalls/sysinfo.rs:188-210`) gives `EINVAL` when the resource number does not fit `u32` (`detcore/src/syscalls/sysinfo.rs:193`) and `EFAULT` for a `NULL` pointer (`detcore/src/syscalls/sysinfo.rs:194`). Next it reads the struct from guest memory, then gives `EINVAL` for a resource number outside the table (`detcore/src/syscalls/sysinfo.rs:204`).
- `validate_resource_limit_mutation` (`detcore/src/syscalls/sysinfo.rs:131-157`), which `prlimit64` uses too, applies these rules in order:
  - soft above hard is `EINVAL` (`detcore/src/syscalls/sysinfo.rs:136`, F17);
  - an exact no-op is accepted for every resource (`detcore/src/syscalls/sysinfo.rs:142`, F22);
  - any real change to a resource other than `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_CORE` is `EPERM` (`detcore/src/syscalls/sysinfo.rs:147-152`);
  - raising the hard limit is `EPERM` (`detcore/src/syscalls/sysinfo.rs:153`, F18).
  The table is written only when the value changed (`detcore/src/syscalls/sysinfo.rs:206`).
- `getrlimit` and `prlimit64` read the same table (`detcore/src/syscalls/sysinfo.rs:163`, `detcore/src/syscalls/sysinfo.rs:222`), so the three calls agree by construction.

Why `model`:

- Forwarding the call and returning the kernel's answer would let host limits, capabilities and `nr_open` decide the result, which are the nondeterminism sources above. Under the configure route the virtual table still decides the answer; the host limit is set to match it, never consulted for it.
- The virtual table alone (today's `emulate`) is not enough: an accepted limit that nothing enforces, and a reported limit the host undercuts, are exactly what owner decision 5 rules out.
- `refuse` would break shells and runtimes that lower `RLIMIT_CORE` or `RLIMIT_STACK`, or set `RLIMIT_NOFILE`, at startup. The comment at `detcore/src/syscalls/sysinfo.rs:145-146` gives sanitizers lowering the `RLIMIT_CORE` soft limit as the example.
- Refusing every hard-limit raise matches what Linux gives the guest: the guest runs as root inside a user namespace, and `capable(CAP_SYS_RESOURCE)` checks the initial namespace. The virtual `RLIMIT_NOFILE` hard limit of 1048576 is also the default `nr_open`, so a raise above it is `EPERM` on Linux for that reason too (F19).

How the emulation differs from Linux at this SHA:

- **Changes refused for 13 resources.** Linux lets any process lower `RLIMIT_AS`, `RLIMIT_CPU`, `RLIMIT_FSIZE`, `RLIMIT_NPROC` and the other nine resources, and Detcore returns `EPERM` (F1-F3, F6, F7, F9-F16). The doc comment at `detcore/src/syscalls/sysinfo.rs:212-219` gives the reason: an accepted change is guest-visible compatibility state only, and Detcore does not ask the host kernel to enforce it. Refusing is honest where a lowered limit would never trigger the `SIGXCPU`, `SIGXFSZ` or `ENOMEM` that the program expects. Shell commands such as `ulimit -v`, `ulimit -t` or `ulimit -f` therefore fail under Hermit.
- **Accepted changes are not enforced.** The table is read only by the getrlimit, setrlimit and prlimit64 handlers and copied at clone: a search for `resource_limits` in `detcore/src` finds no other user. By this reading of the code, lowering the `RLIMIT_NOFILE` soft limit to 64 does not make the 65th descriptor fail with `EMFILE`. The real host limit (524288 on the measuring host, `tests/c/rlimit_identity.c:38-40`) is still in force and is below the virtual 1048576, so a guest that relies on its reported limit hits a host-dependent `EMFILE` first, handed to it unmarked (`detcore/src/syscalls/files.rs:1130`-`1132`; F25). This is a hypothesis from the code; T8 and T9 measure it.
- **Order of checks.** A negative resource number fails the `u32` conversion before the struct is read, so a negative resource with a bad pointer gives `EINVAL`, where Linux copies first and gives `EFAULT` (F20, F21). A positive invalid resource reads memory first, as Linux does.
- **`RLIMIT_STACK` and `RLIMIT_CORE` have no host effect.** `execve` does not see the virtual `RLIMIT_STACK`, and the virtual `RLIMIT_CORE` does not change whether the host writes a core dump (`detcore/src/syscalls/sysinfo.rs:145-146`).

Target (owner decision 6, 2026-10-04): as an emulated call, `handle_setrlimit` makes Linux's checks in the kernel's order. `setrlimit` in `kernel/sys.c` copies the struct first (`EFAULT`, F21), then calls `do_prlimit`, which checks in turn: `resource >= RLIM_NLIMITS`, with the argument unsigned (`EINVAL`, F20); `rlim_cur > rlim_max` (`EINVAL`, F17); an `RLIMIT_NOFILE` hard limit above `nr_open` (`EPERM`, F19); and a hard-limit raise without `CAP_SYS_RESOURCE` (`EPERM`, F18). The handler should move the `u32` conversion of `resource` (`detcore/src/syscalls/sysinfo.rs:193`) after the memory read, so a negative resource with an unreadable pointer gives `EFAULT`. `validate_resource_limit_mutation` already keeps `EINVAL` for soft above hard ahead of every `EPERM` (`detcore/src/syscalls/sysinfo.rs:136`). T5 pins the order.

Record and replay use the same handler, which never touches the kernel, so a replay recomputes the same answers from the same table.

## Recommended tests

- T1 (covers F8, F17, F18, F19, F22): with raw `syscall(SYS_setrlimit, ...)`, lower the `RLIMIT_NOFILE` soft limit, read it back, raise it to the hard limit again, then check that soft above hard gives `EINVAL` and hard + 1 gives `EPERM`.
  Existing candidate: `tests/c/rlimit_identity.c`, cell `c-programs/rlimit-identity` (`tests/e2e/manifests/c-programs.yaml:14992`, verify ci on ptrace). It prints the virtual hard limit and every value it read back. Its hard + 1 request is also above the default `nr_open`, but it cannot tell F18 from F19, because both give `EPERM`.
- T2 (covers F8, F22): the raw `SYS_setrlimit` lowering and restore of the `RLIMIT_NOFILE` soft limit in `check_limit_mutations` (`tests/c/resource_determinism.c:155-173`).
  Existing candidate: cell `c-programs/resource-determinism` (`tests/e2e/manifests/c-programs.yaml:7549`, verify ci on ptrace).
- T3 (covers F4, F5, F23): with raw `SYS_setrlimit`, lower the `RLIMIT_STACK` soft limit and restore it to the 8 MiB hard limit. Lower the `RLIMIT_CORE` soft limit to 0, then its hard limit, and check that raising the lowered hard limit again gives `EPERM`. Check that a finite hard limit with an infinite soft limit gives `EINVAL`.
- T4 (covers F1, F2, F3, F6, F7, F9, F10, F11, F12, F13, F14, F15, F16, F22): for each of the 13 resources Detcore restricts today, setting the current pair again returns 0; lowering the soft limit by one returns 0 and raw `SYS_getrlimit` reads the new pair back; raising the soft limit back to the hard limit returns 0; lowering the hard limit by one returns 0; and raising the hard limit again returns `EPERM`. `RLIMIT_MEMLOCK` starts at 0 soft and hard, so for it only the no-op (0) and a hard raise (`EPERM`) apply. By the code this fails at this SHA, which returns `EPERM` for every real change to these resources (`detcore/src/syscalls/sysinfo.rs:147`-`152`).
- T5 (covers F20, F21): with a valid pointer, resource `RLIM_NLIMITS` and resource -1 give `EINVAL`. With a valid resource, a `NULL` pointer and the pointer `(void *)1` give `EFAULT`. Resource -1 with pointer `(void *)1` gives `EFAULT` natively and must give `EFAULT` under Hermit (owner decision 6); the code at this SHA suggests `EINVAL`.
- T6 (covers F24): after a raw `SYS_setrlimit` on `RLIMIT_NOFILE`, a sibling thread sees the new value, a forked child inherits it, a change in the child does not reach the parent, and a program started with `execve` sees it.
  Related existing coverage: `check_prlimit_fork_inheritance` (`tests/c/resource_determinism.c:259`) checks fork inheritance through `prlimit64`, not `setrlimit`, and checks neither threads nor `execve`.
- T7 (covers F8, F22): a raw `SYS_setrlimit` that only changes `RLIMIT_NOFILE` must leave every other resource's pair unchanged, read back with raw `SYS_getrlimit` for all 16 resources.
- T8 (covers F8): lower the `RLIMIT_NOFILE` soft limit to 16 with raw `SYS_setrlimit`, then open descriptors until one fails. Assert `EMFILE` exactly when the next descriptor would be 16, as natively (owner decision 5 target). By the code this fails at this SHA, because the lowered limit is not enforced; file that as a defect against the non-enforcing emulation.
- T9 (covers F25): launch Hermit under a lowered host limit (`prlimit --nofile=64:64` on the launcher). The guest lowers its soft `RLIMIT_NOFILE` to 1000 with raw `SYS_setrlimit` (accepted: below the virtual hard limit) and opens descriptors until one fails. Assert that the run reports `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=openat errno=EMFILE` and ends with the infrastructure-error exit convention (exit 122 with `class=infrastructure-error cause=host-rlimit` on stderr, as proposed in the error-handling convention), not with the guest seeing `EMFILE` near descriptor 64. Today the guest gets the bare `EMFILE`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/457: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/465 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (Detcore keeps a per-process virtual limit table with fixed initial values and never calls the kernel; real changes are accepted only for RLIMIT_STACK, RLIMIT_NOFILE and RLIMIT_CORE without raising the hard limit, and are not enforced); linux_determinism stays conditional; merged duplicate https://github.com/rrnewton/hermit/issues/465 and reconciled its stale pass-through claim; 24 flag behaviours and 8 tests listed; three open questions (restricted resources, enforcement, error order), so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (every accepted or default limit is the limit in force, enforced by Detcore or configured on the host per resource; lowering accepted once a resource reaches it; interim host-rlimit infrastructure error, passed through today at files.rs:1130); target_policy emulate -> model; T8 now asserts EMFILE at 16; new F25, T9; removed the restricted-resources and enforcement questions; added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (Target: copy-in EFAULT before the resource EINVAL, then soft>hard EINVAL, nr_open EPERM, hard-raise EPERM, per setrlimit/do_prlimit in kernel/sys.c; T5 now asserts EFAULT for resource -1 with a bad pointer); target_policy stays model; no new F/T ids; removed the negative-resource check-order question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (soft equal to hard at start; a lowering of any of the 16 resources is forwarded to the host and recorded; a hard raise is EPERM; a soft raise back up to the hard limit is accepted and forwarded, F22); target_policy stays model; T3 restores RLIMIT_STACK to its 8 MiB hard limit; T4 now asserts that lowerings succeed; removed the standing-policy proposal; new open question on the five resources the host counts beyond the guest (CPU, RTTIME, NPROC, SIGPENDING, MSGQUEUE)
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/rlimit_identity.c, T2 covered by tests/c/resource_determinism.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setrlimit`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setrlimit&body=Discussion+for+the+%60setrlimit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetrlimit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setrlimit%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setrlimit" (https://github.com/rrnewton/hermit/issues/457), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/465, filed the same day under the same title (legacy variant A1), is merged into the sections above. Its comment history (0 comments) stays on that issue, as does that of https://github.com/rrnewton/hermit/issues/457 (0 comments).

### Open questions

- Forwarding is not enough for five resources, because the host counts them in a way the guest does not control. `RLIMIT_CPU` and `RLIMIT_RTTIME` are measured in host CPU time, so a forwarded lowering raises `SIGXCPU` at a host-dependent point. `RLIMIT_NPROC`, `RLIMIT_SIGPENDING` and `RLIMIT_MSGQUEUE` count everything the real user ID holds on the host, not only the guest's share. For these, Detcore has to compare a failure or signal with its own count (virtual CPU time, guest processes, queued signals) and report a mismatch as `cause=host-rlimit`, or enforce the limit itself. The implementation should say which, per resource.

### Reconciled claims from the legacy texts

- Both legacy texts say Hermit routes `setrlimit` through the temporary pass-through block of `detcore/src/syscall_classification.rs`, which was true at `dd60278f`. At hermit f4055eb9cae4 the typed arm at `detcore/src/lib.rs:2781` sends it to `handle_setrlimit`, which never calls the kernel. The code wins, and Hermit design describes it.
- https://github.com/rrnewton/hermit/issues/465 says the get path of `getrlimit` and `prlimit64` leaks host limits and that `setrlimit` should be handled together with them. At this SHA all three read the same fixed virtual table (`detcore/src/syscalls/sysinfo.rs:163`, `detcore/src/syscalls/sysinfo.rs:188`, `detcore/src/syscalls/sysinfo.rs:222`), so the leak is gone and the joint handling has happened. `tests/c/rlimit_identity.c:38-40` measures native hard `RLIMIT_NOFILE` at 524288 and Hermit's at 1048576.
- Both texts recommended NEEDS-HANDLER. The handler exists; the policy it realizes is `emulate`.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/457)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setrlimit`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setrlimit` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setrlimit(RLIMIT_NOFILE, {rlim_cur=512*1024, rlim_max=512*1024}) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/465)

Evidence gathered on the same host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setrlimit` on a direct `syscall()` harness, invoked with non-destructive query, no-op or failing arguments. Setting `RLIMIT_CORE` to its current value returned 0.

Observed strace line:
```text
setrlimit(RLIMIT_CORE, {rlim_cur=RLIM64_INFINITY, rlim_max=RLIM64_INFINITY}) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
