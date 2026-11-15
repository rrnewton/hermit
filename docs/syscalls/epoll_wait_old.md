---
schema: syscall-pages/v1
syscall: epoll_wait_old
nr_x86_64: 215
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_wait_old&body=Discussion+for+the+%60epoll_wait_old%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_wait_old.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_wait_old%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`epoll_wait_old` is an obsolete x86_64 syscall number (215) with no implementation behind it in current Linux kernels; calling it returns `ENOSYS`. At hermit f4055eb9cae4, Detcore does not return `ENOSYS`. Its typed arm panics with "Not handling deprecated syscall" (`detcore/src/lib.rs:2626`-`2629`), so any guest that calls it brings down the Hermit run. The intended policy is a fixed `ENOSYS` refusal, the same fix https://github.com/rrnewton/hermit/pull/549 applied to the sibling `epoll_ctl_old`.

## Linux semantics

The number was reserved for an early form of `epoll_wait`. Current x86_64 kernels have no implementation behind it, and the call returns `ENOSYS` for any arguments, with no side effect and without blocking. There is no man page; the Reverie typed struct `EpollWaitOld {}` is documented only as "Undocumented" (`reverie-syscalls/src/syscalls.rs:2208` at Reverie d766df20b780). Programs reach it only through a raw `syscall(215, ...)`. glibc does not call it.

Host probe on the agent host (Linux 7.1.3, python ctypes, 2026-10-03), recorded by this body's author: raw syscall 215 returned -1 with `errno` `ENOSYS`.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

`linux_determinism: deterministic`. The kernel returns `ENOSYS` without reading its arguments or any process or host state, so every call on every run gives the same answer. This changes the rendered placeholder `conditional`.

A kernel built with a different syscall table could in principle give a different answer. No mainline x86_64 kernel that Hermit supports does.

## Hermit design

Target policy: `refuse`, with a fixed `ENOSYS`. That answer matches Linux, does not depend on the host, and is the same for run, record and replay.

What the code does today:

- The typed arm is `Syscall::EpollWaitOld(s) => panic!("Not handling deprecated syscall: {}", ...)` (`detcore/src/lib.rs:2626`-`2629`). The guest gets no errno; Hermit itself fails. Every run fails the same way, but the run is lost, and Linux would let the program continue after `ENOSYS`.
- The syscall is subscribed (`detcore/src/lib.rs:1210`) and is in the `Determinized` list (`detcore/src/syscall_classification.rs:86`).
- It is not in the deterministic-refusal aggregate `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`). The sibling `epoll_ctl_old` is (`detcore/src/syscall_classification.rs:1345`). This is why `code_facts.refused` is false.

The code does not yet realize `target_policy`. The fix mirrors `epoll_ctl_old` (`detcore/src/lib.rs:2630`-`2633`):

- Replace the panic with `Err(Error::Errno(Errno::ENOSYS))`.
- Add `Sysno::epoll_wait_old` to the `matches!` list in `is_deterministically_refused_syscall`, so backends that run guest syscalls outside Detcore's dispatcher refuse it too.
- Add it to the expected members in the unit test `deterministic_refusal_aggregate_includes_fixed_error_families` (`detcore/src/syscall_classification.rs:2710`).

This defect needs a hermit issue; this agent had no GitHub access and could not file one.

Target (owner decision 10): return `ENOSYS` now, in place of the panic at `detcore/src/lib.rs:2626`-`2629`. The decision names this call explicitly, and says that any panic guest input can reach is a defect; this arm is one, reached by a single raw `syscall(215, ...)` with any arguments. Per owner decision 6 the refusal is one fixed errno returned before any argument is read, which is also what Linux does, so no argument order is owed. Nothing has to fall back: glibc and musl never call this number, and a program that calls it raw on any current kernel already gets `ENOSYS` and must cope. Today's code differs only in that arm and in the missing aggregate entry above; `target_policy` was already `refuse`.

`refuse` fits better than the alternatives. `passthrough` would also return `ENOSYS` from the kernel, but it would make the answer depend on the host kernel's table. `model` and `emulate` have nothing to model.

## Recommended tests

- T1 (covers none): raw `syscall(215, ...)` with an epoll descriptor and an events buffer, and again with garbage arguments, must return -1 with `errno` `ENOSYS` under `hermit run --verify` on ptrace. The guest must continue afterwards and exit 0. At f4055eb9cae4 this test fails, because Hermit panics. Assert also that Hermit's exit status is the guest's 0, not a tracer failure. The natural home is next to the `epoll_ctl_old` check in the `control-fds` scenario of `tests/c/epoll_determinism.c` (`tests/c/epoll_determinism.c:404`-`409`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (fixed ENOSYS, matching Linux); linux_determinism conditional -> deterministic (the kernel has no implementation and always returns ENOSYS); the code does not realize refuse yet: lib.rs:2626 panics, and the syscall is missing from the refusal aggregate at syscall_classification.rs:1319, so refused is false; no flags; T1 proposed; tracking_status stays todo pending the fix and a hermit issue
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 10 and 6 applied (ENOSYS now in place of the guest-reachable panic at lib.rs:2626-2629, fixed errno before any argument); target_policy stays refuse; no new F/T ids, T1 now also asserts Hermit's exit status; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_wait_old`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_wait_old&body=Discussion+for+the+%60epoll_wait_old%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_wait_old.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_wait_old%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_wait_old`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

https://github.com/rrnewton/hermit/pull/549 (commit 3c49d197b4) replaced the matching panic for `epoll_ctl_old` with `ENOSYS` but left this arm unchanged.

No test at f4055eb9cae4 names `epoll_wait_old`; `code_facts.tests_naming` is 0.
