---
schema: syscall-pages/v1
syscall: landlock_add_rule
nr_x86_64: 445
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/626
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+landlock_add_rule&body=Discussion+for+the+%60landlock_add_rule%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flandlock_add_rule.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60landlock_add_rule%60%2C+or+in+what+the+page+says%3F%0A
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

`landlock_add_rule(ruleset_fd, rule_type, rule_attr, flags)` adds one allow rule (a file hierarchy or a TCP port) to a Landlock ruleset created by `landlock_create_ruleset`. Detcore never forwards it to the host: at hermit f4055eb9cae4 it returns a fixed `ENOSYS` (`detcore/src/lib.rs:2322-2324`), the answer of a kernel built without Landlock.

Manual reference: Linux man-pages landlock_add_rule(2); UAPI header `linux/landlock.h`.

## Linux semantics

The call changes only the ruleset object behind `ruleset_fd`; nothing is enforced until `landlock_restrict_self`. A rule grants `allowed_access` rights, which must be a non-empty subset of the rights the ruleset handles, on either a file hierarchy or a TCP port. glibc has no wrapper.

In current kernels the checks run in this order: Landlock availability (`ENOSYS` when not built, `EOPNOTSUPP` when built but not enabled at boot), then `flags`, then `ruleset_fd`, then the rule type and its attribute.

A path-beneath rule names its hierarchy by an open descriptor (`parent_fd`, typically opened with `O_PATH`). The kernel ties the rule to that inode, so its later effect depends on which host objects the descriptor refers to, not on the path string.

### Flags and options

- F1: `rule_type == LANDLOCK_RULE_PATH_BENEATH`, `rule_attr` is `struct landlock_path_beneath_attr { allowed_access, parent_fd }`: grants `allowed_access` (a subset of the ruleset's `handled_access_fs`, else `EINVAL`) on the hierarchy under `parent_fd`. `parent_fd` not open gives `EBADF`; a descriptor on an internal filesystem such as a pipe or socket gives `EBADFD`.
- F2: `rule_type == LANDLOCK_RULE_NET_PORT`, `rule_attr` is `struct landlock_net_port_attr { allowed_access, port }` (ABI 4): grants `BIND_TCP` and/or `CONNECT_TCP` on one port. `allowed_access` outside `handled_access_net` or a `port` above 65535 gives `EINVAL`; `EAFNOSUPPORT` when the kernel has no TCP/IP support.
- F3: `allowed_access == 0` for either rule type: `ENOMSG`.
- F4: any other `rule_type`: `EINVAL`.
- F5: `flags != 0`: `EINVAL` (no flags are defined).
- F6: `ruleset_fd` checks: not an open descriptor gives `EBADF`; a descriptor that is not a Landlock ruleset gives `EBADFD`; a ruleset descriptor without write access gives `EPERM`.
- F7: `rule_attr` not readable: `EFAULT`.
- F8: availability: `ENOSYS` on a kernel without Landlock, `EOPNOTSUPP` when Landlock is built but not enabled at boot; returned before any other check.

## Determinism analysis

`linux_determinism: conditional`. For one kernel and one set of descriptors, the result is fixed: the call mutates only a private ruleset object and its errors are functions of the arguments. Across hosts it differs:

- availability (F8) depends on the kernel build and the boot LSM list. On the measurement host, measured 2026-10-03 (kernel 7.1.3, `CONFIG_SECURITY_LANDLOCK` not set), a native `syscall(445, -1, 1, NULL, 0)` returned `-1`/`ENOSYS`; on a Landlock kernel the same call returns `EBADF`;
- the net rule type (F2) and the valid `allowed_access` bits depend on the kernel's ABI version;
- a path rule binds to whatever inode `parent_fd` names on the host filesystem, so the effect after enforcement depends on host filesystem layout.

Under Hermit the answer is a constant `ENOSYS`.

## Hermit design

Target policy: `refuse`, which is what the code does at hermit f4055eb9cae4.

- `classify_syscall` lists `landlock_add_rule` as Determinized (`detcore/src/syscall_classification.rs:465`), and `is_landlock_sandbox_syscall` (`detcore/src/syscall_classification.rs:1121-1125`) selects it.
- The arm at `detcore/src/lib.rs:2322-2324` returns `ENOSYS` before the typed match, independent of `panic_on_unsupported_syscalls` (`detcore/src/lib.rs:1915`) and without a host call. `is_deterministically_refused_syscall` includes it (`detcore/src/syscall_classification.rs:1327`), so backends that run guest syscalls outside Detcore's dispatcher apply the same refusal (comment at `detcore/src/syscall_classification.rs:2714-2717`).
- `ENOSYS` comes before every argument check, matching a kernel without Landlock: a call with a bad `ruleset_fd` (F6), bad flags (F5) or a bad type (F4) also gets `ENOSYS`. This is consistent with `landlock_create_ruleset` also returning `ENOSYS`, so a guest can never hold a ruleset descriptor to pass here.

Why not the alternatives: `passthrough` is unusable on its own because Hermit never lets the guest obtain a ruleset descriptor, and forwarding would expose host ABI and filesystem identity; `emulate` (accepting rules into a fake ruleset) only makes sense together with a fake `landlock_restrict_self`, which would claim a sandbox that is not enforced; `model` would require Detcore to enforce rules itself. See landlock_create_ruleset (https://github.com/rrnewton/hermit/issues/627) for the family decision.

## Recommended tests

- T1 (covers F1, F6, F8): under `hermit run --verify` on ptrace, `landlock_add_rule(-1, LANDLOCK_RULE_PATH_BENEATH, NULL, 0)` returns `-1`/`ENOSYS` in both runs, and so does the same call with a valid `O_PATH` `parent_fd` and a non-ruleset `ruleset_fd` (a regular file).
  Existing guest: `tests/rust/landlock.rs:54-65` (cargo bin `rustbin_landlock`, `tests/Cargo.toml:86-89`), which covers only the `-1` case. At this SHA no cargo integration test and no e2e manifest program runs it (only the internal Buck glob at `tests/BUCK:415`), so it needs a ptrace runner before it can be recorded as coverage.
- T2 (covers F2, F3, F4, F5, F7): `LANDLOCK_RULE_NET_PORT` with `port = 70000`, `allowed_access = 0`, `rule_type = 99`, `flags = 1`, and a `rule_attr` pointing to an unmapped page: each returns `ENOSYS`, never `EINVAL`, `ENOMSG` or `EFAULT`.
- T3 (covers F1): after a refused path-beneath call naming a directory, the guest can still create and read files under that directory, showing that no host restriction was recorded or installed.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/626: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_landlock_sandbox_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2322-2324 returns a fixed ENOSYS before any argument check, matching a kernel without Landlock); linux_determinism stays conditional (availability, ABI version and host inode identity decide the native result); 8 flag behaviours and 3 tests listed; tests/rust/landlock.rs has no ptrace runner at this SHA, so no coverage token and tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `landlock_add_rule`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+landlock_add_rule&body=Discussion+for+the+%60landlock_add_rule%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flandlock_add_rule.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60landlock_add_rule%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: landlock_add_rule" (https://github.com/rrnewton/hermit/issues/626), legacy variant B. The determinized ENOSYS arm carries `TODO-HUMAN-REVIEW(#827)` in the code (https://github.com/rrnewton/hermit/issues/827). Sibling bodies: landlock_create_ruleset (https://github.com/rrnewton/hermit/issues/627) and landlock_restrict_self (https://github.com/rrnewton/hermit/issues/628) share the same dispatch arm.

### Notes from the P3 reading (2026-10-03)

- `tests/e2e/manifests/inventory/test-files.json:3593-3595` says `tests/rust/landlock.rs` is "selected by a Rust integration test", but no file under `hermit-cli/tests/` or `detcore/tests/` names `rustbin_landlock` at f4055eb9cae4.
- The unit test `landlock_sandbox_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2344`) checks classification only and starts no Hermit run.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
