---
schema: syscall-pages/v1
syscall: epoll_ctl_old
nr_x86_64: 214
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl_old&body=Discussion+for+the+%60epoll_ctl_old%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl_old.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl_old%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`epoll_ctl_old` is an obsolete x86_64 syscall number (214) with no implementation behind it in current Linux kernels; calling it returns `ENOSYS`. Detcore refuses it with a fixed `ENOSYS` and never consults the host (`detcore/src/lib.rs:2633` at hermit f4055eb9cae4). That matches Linux exactly.

## Linux semantics

The number was reserved for an early form of `epoll_ctl`. Current x86_64 kernels have no implementation behind it, and the call returns `ENOSYS` for any arguments, with no side effect. There is no man page; the Reverie typed struct `EpollCtlOld {}` is documented only as "Undocumented" (`reverie-syscalls/src/syscalls.rs:2201` at Reverie d766df20b780). Programs reach it only through a raw `syscall(214, ...)`. glibc does not call it.

Host probe on the agent host (Linux 7.1.3, python ctypes, 2026-10-03), recorded by this body's author: raw syscall 214 returned -1 with `errno` `ENOSYS`.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

`linux_determinism: deterministic`. The kernel returns `ENOSYS` without reading its arguments or any process or host state, so every call on every run gives the same answer. This changes the rendered placeholder `conditional`.

A kernel built with a different syscall table could in principle give a different answer. No mainline x86_64 kernel that Hermit supports does.

## Hermit design

Target policy: `refuse`. The fixed answer Hermit returns is the same one Linux gives, so refusing is also faithful.

What the code does today:

- The typed arm returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall (`detcore/src/lib.rs:2630`-`2633`). The comment there says "The obsolete x86_64 entry point is absent from modern Linux kernels."
- The syscall is a member of the deterministic-refusal aggregate `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`; member at `detcore/src/syscall_classification.rs:1345`). The comment on the unit test `deterministic_refusal_aggregate_includes_fixed_error_families` (`detcore/src/syscall_classification.rs:2710`) says backends that run guest syscalls outside Detcore's dispatcher consult this aggregate. That test lists `epoll_ctl_old` among the expected members (`detcore/src/syscall_classification.rs:2738`).
- It is subscribed (`detcore/src/lib.rs:1211`) and is in the `Determinized` list (`detcore/src/syscall_classification.rs:75`).
- Before https://github.com/rrnewton/hermit/pull/549 (commit 3c49d197b4) this arm panicked. That pull request changed it to return `ENOSYS`.

Because no syscall reaches the host, record and replay return the same fixed answer. The code already realizes `refuse`, and `code_facts.refused` is true.

## Recommended tests

- T1 (covers none): raw `syscall(SYS_epoll_ctl_old, ...)` with valid arguments (an epoll descriptor, `EPOLL_CTL_ADD`, an eventfd, NULL) and with garbage arguments (-1 and huge values) must return -1 with `errno` `ENOSYS`, under `hermit run --verify` on ptrace. The epoll descriptor must stay usable afterwards.
  Existing candidate: `tests/c/epoll_determinism.c:405`-`409` makes the valid-argument call inside `run_control_fds` (`tests/c/epoll_determinism.c:343`), the `control-fds` scenario. The Rust test `notification_control_syscalls_are_deterministic` (`hermit-cli/tests/epoll_determinism.rs:399`) runs that scenario under `hermit run` without a `--backend` flag. The scenario is also listed in `tests/e2e/lib/determinism-stress/syscalls.sh:17`. The manifest cell `c-programs/epoll-determinism` (`tests/e2e/manifests/c-programs.yaml:1318`) is enabled on ptrace (line 1347), but its ptrace `guest_args` run only `multi` (lines 1356-1358), not `control-fds`. Before T1 can be recorded as covered, someone must confirm that one of these runs on ptrace in CI. A garbage-argument case would also need to be added.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (fixed ENOSYS without consulting the host, lib.rs:2633, member of the refusal aggregate at syscall_classification.rs:1345), which matches Linux; linux_determinism conditional -> deterministic (the kernel has no implementation and always returns ENOSYS); no flags; T1 has a candidate in tests/c/epoll_determinism.c:406, but no ptrace CI run of that scenario is confirmed, so no coverage is recorded and tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_ctl_old`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl_old&body=Discussion+for+the+%60epoll_ctl_old%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl_old.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl_old%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_ctl_old`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The arm carries `TODO-HUMAN-REVIEW(#549)`. That tag names https://github.com/rrnewton/hermit/pull/549 ("Document and verify notification fd determinism", commit 3c49d197b4). Its commit message says it made Hermit "return ENOSYS for the obsolete epoll_ctl entry point instead of panicking".

The sibling `epoll_wait_old` (215) still panics at f4055eb9cae4; see that syscall's body.
