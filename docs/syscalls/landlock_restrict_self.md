---
schema: syscall-pages/v1
syscall: landlock_restrict_self
nr_x86_64: 446
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/628
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+landlock_restrict_self&body=Discussion+for+the+%60landlock_restrict_self%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flandlock_restrict_self.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60landlock_restrict_self%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_landlock_sandbox_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`landlock_restrict_self(ruleset_fd, flags)` enforces a Landlock ruleset on the calling thread, stacking a new, irrevocable domain that its future children inherit. Detcore never forwards it to the host: at hermit f4055eb9cae4 it returns a fixed `ENOSYS` (`detcore/src/lib.rs:2322-2324`), the answer of a kernel built without Landlock, so no host LSM domain is ever installed on a guest thread.

Manual reference: Linux man-pages landlock_restrict_self(2); UAPI header `linux/landlock.h`.

## Linux semantics

The caller must have `no_new_privs` set (`prctl(PR_SET_NO_NEW_PRIVS, 1)`) or hold `CAP_SYS_ADMIN` in its user namespace. On success the kernel merges the ruleset into a new domain stacked on top of the thread's current one, and installs it in the calling thread's credentials. Other existing threads of the process are not restricted. Children created afterwards inherit the domain, and it survives `execve`. Nothing can remove it.

After enforcement the kernel denies, in the restricted threads and their descendants, any handled access not granted by a rule: denied filesystem operations fail with `EACCES` (`EXDEV` for a forbidden rename or link across hierarchies), denied TCP `bind`/`connect` with `EACCES`, scoped signals and abstract UNIX socket connections to processes outside the domain with `EPERM`. A sandboxed process also cannot ptrace a process outside its domain. Denials can be logged through the audit subsystem.

glibc has no wrapper. Availability is decided before any argument: `ENOSYS` on a kernel without Landlock, `EOPNOTSUPP` when it is built but not enabled at boot.

### Flags and options

- F1: `flags == 0` with a valid ruleset descriptor: stacks and enforces a new domain on the calling thread only; inherited by children created afterwards and kept across `execve`; irrevocable.
- F2: privilege precondition: without `no_new_privs` and without `CAP_SYS_ADMIN` in the current user namespace the call fails with `EPERM`.
- F3: `LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF` (ABI 7): do not audit-log denials from this thread and its children while they run the same executable.
- F4: `LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON` (ABI 7): audit-log denials after an `execve` inside the new domain.
- F5: `LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF` (ABI 7): mute logging for domains nested later under this one. With `ruleset_fd == -1` and this flag alone, it changes only the logging setting and creates no domain.
- F6: an unknown `flags` bit (or a logging flag on a kernel older than ABI 7): `EINVAL`.
- F7: `ruleset_fd` checks: not an open descriptor (including `-1` without `LOG_SUBDOMAINS_OFF` alone) gives `EBADF`; not a Landlock ruleset gives `EBADFD`; a ruleset descriptor without read access gives `EPERM`.
- F8: stacking limit: a thread may carry at most 16 nested domains; one more gives `E2BIG`.
- F9: enforcement effects after success: denied filesystem accesses (`EACCES`, `EXDEV` for cross-hierarchy rename or link), denied TCP bind and connect (`EACCES`), scoped signals and abstract UNIX connects (`EPERM`), ptrace of processes outside the domain refused.
- F10: availability: `ENOSYS` on a kernel without Landlock, `EOPNOTSUPP` when built but not enabled at boot; returned before any other check.

## Determinism analysis

`linux_determinism: conditional`. With a fixed kernel, credentials and filesystem, the call and its later effects are deterministic: the domain is a pure function of the ruleset, and each later access check is a function of the domain and the object. It does not repeat across hosts:

- availability (F10), the logging flags (F3-F6) and the rights a domain can handle depend on the kernel build, boot LSM list and ABI version. On the measurement host, measured 2026-10-03 (kernel 7.1.3, `CONFIG_SECURITY_LANDLOCK` not set), a native `syscall(446, -1, 0)` returned `-1`/`ENOSYS`; a Landlock kernel returns `EPERM` or `EBADF` for the same call;
- path rules resolve to host inodes, so which later accesses are denied (F9) depends on host filesystem layout;
- audit logging is a host-visible side effect outside the guest.

A domain installed through passthrough would also be guest state that lives in the host kernel and that Detcore does not track: later guest accesses would fail with `EACCES` decided by the host LSM, not by Detcore. Under Hermit the call returns a constant `ENOSYS` and installs nothing.

## Hermit design

Target policy: `refuse`, which is what the code does at hermit f4055eb9cae4.

- `classify_syscall` lists `landlock_restrict_self` as Determinized (`detcore/src/syscall_classification.rs:466`), and `is_landlock_sandbox_syscall` (`detcore/src/syscall_classification.rs:1121-1125`) selects it.
- The arm at `detcore/src/lib.rs:2322-2324` returns `ENOSYS` before the typed match, independent of `panic_on_unsupported_syscalls` (`detcore/src/lib.rs:1915`) and without a host call, so no domain is installed and no audit record is produced. `is_deterministically_refused_syscall` includes it (`detcore/src/syscall_classification.rs:1327`), so backends that run guest syscalls outside Detcore's dispatcher apply the same refusal (comment at `detcore/src/syscall_classification.rs:2714-2717`).
- `ENOSYS` comes before every check, including the privilege check (F2), matching a kernel without Landlock.

Why not the alternatives: `passthrough` would install a host LSM domain that changes later guest syscall results outside Detcore's knowledge, and its availability is host-dependent; `emulate` as a no-op success would claim a sandbox that does not exist; `model` would require Detcore to enforce domains (paths, ports, signals, sockets, ptrace) itself, which is not planned. See landlock_create_ruleset (https://github.com/rrnewton/hermit/issues/627) for the family decision.

The code comments at `detcore/src/lib.rs:2314-2315` and `detcore/src/syscall_classification.rs:1112-1113` say a ruleset "restricts the whole thread tree". On Linux it restricts the calling thread and the tasks it creates afterwards, not sibling threads that already exist (F1). The refusal does not depend on that wording.

## Recommended tests

- T1 (covers F1, F7, F10): under `hermit run --verify` on ptrace, `landlock_restrict_self(-1, 0)` returns `-1`/`ENOSYS` in both runs, and so does the call with a regular-file descriptor as `ruleset_fd`.
  Existing guest: `tests/rust/landlock.rs:67-70` (cargo bin `rustbin_landlock`, `tests/Cargo.toml:86-89`), which covers only the `-1` case. At this SHA no cargo integration test and no e2e manifest program runs it (only the internal Buck glob at `tests/BUCK:415`), so it needs a ptrace runner before it can be recorded as coverage.
- T2 (covers F2, F3, F4, F5, F6): with and without `PR_SET_NO_NEW_PRIVS`, call with each logging flag, with `-1` plus `LOG_SUBDOMAINS_OFF`, and with an unknown bit `0x80`: every call returns `ENOSYS`, never `EPERM`, `EINVAL` or 0.
- T3 (covers F8, F9): call it 17 times (all `ENOSYS`, no `E2BIG`), then show nothing was enforced: the guest still opens and creates files, binds a TCP port, and a forked child that `execve`s a helper can do the same; output is identical across both `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/628: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_landlock_sandbox_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2322-2324 returns a fixed ENOSYS before any check and installs no host LSM domain); linux_determinism stays conditional (availability, ABI version and host inode identity decide native results); 10 flag behaviours and 3 tests listed; tests/rust/landlock.rs has no ptrace runner at this SHA, so no coverage token and tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `landlock_restrict_self`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+landlock_restrict_self&body=Discussion+for+the+%60landlock_restrict_self%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flandlock_restrict_self.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60landlock_restrict_self%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: landlock_restrict_self" (https://github.com/rrnewton/hermit/issues/628), legacy variant B. The determinized ENOSYS arm carries `TODO-HUMAN-REVIEW(#827)` in the code (https://github.com/rrnewton/hermit/issues/827). Sibling bodies: landlock_create_ruleset (https://github.com/rrnewton/hermit/issues/627) and landlock_add_rule (https://github.com/rrnewton/hermit/issues/626) share the same dispatch arm.

### Notes from the P3 reading (2026-10-03)

- `tests/e2e/manifests/inventory/test-files.json:3593-3595` says `tests/rust/landlock.rs` is "selected by a Rust integration test", but no file under `hermit-cli/tests/` or `detcore/tests/` names `rustbin_landlock` at f4055eb9cae4.
- The unit test `landlock_sandbox_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2344`) checks classification only and starts no Hermit run.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
