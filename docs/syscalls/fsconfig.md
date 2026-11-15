---
schema: syscall-pages/v1
syscall: fsconfig
nr_x86_64: 431
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/359
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsconfig&body=Discussion+for+the+%60fsconfig%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsconfig.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsconfig%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`fsconfig(fd, cmd, key, value, aux)` sets parameters on a filesystem context made by `fsopen` or `fspick`, and runs its create and reconfigure commands (new mount API, Linux 5.2). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

Manual reference: Linux man-pages fsconfig(2).

## Linux semantics

The call dispatches on `cmd`. The `FSCONFIG_SET_*` commands pass one parameter, named by `key`, to the filesystem's parameter parser, so which keys and values are accepted depends on the filesystem driver. The `FSCONFIG_CMD_*` commands act on the whole context.

Unlike the other new-mount-API calls, `fsconfig` does not check privilege on entry. The kernel validates in this order:

1. A negative `fd` gives `EINVAL`. The host strace under Additional information, `fsconfig(-1, 0xffffffff, ...) = -1 EINVAL`, shows this check running before the command is examined.
2. Each command checks the shape of `key`, `value` and `aux` (for example, `FSCONFIG_SET_FLAG` needs a key and no value); a mismatch is `EINVAL`. An unknown command is `EOPNOTSUPP`.
3. Then `fd` is looked up: a closed descriptor is `EBADF`, and an open descriptor that is not a filesystem context is `EINVAL`.

Privilege is checked only when a command creates or reconfigures a superblock, which gives `EPERM` to a caller without it. A context in the wrong phase for a command gives `EBUSY`, and a failed create or reconfigure leaves the context unusable. Error details are queued as messages that `read` on the context descriptor returns.

### Flags and options

- F1: `FSCONFIG_SET_FLAG`: set the boolean parameter `key`; `value` must be NULL and `aux` 0.
- F2: `FSCONFIG_SET_STRING`: set `key` to the string `value`; `aux` must be 0.
- F3: `FSCONFIG_SET_BINARY`: set `key` to the binary blob `value` of `aux` bytes.
- F4: `FSCONFIG_SET_PATH`: set `key` to the path `value`, resolved relative to the directory descriptor `aux`.
- F5: `FSCONFIG_SET_PATH_EMPTY`: like F4, but an empty path refers to `aux` itself.
- F6: `FSCONFIG_SET_FD`: set `key` to the open descriptor `aux`.
- F7: `FSCONFIG_CMD_CREATE`: create a superblock, or reuse a matching one, from the configured parameters. Needs privilege over the mount namespace.
- F8: `FSCONFIG_CMD_RECONFIGURE`: apply the parameters to the existing superblock of a context made by `fspick`. Needs privilege over that superblock.
- F9: `FSCONFIG_CMD_CREATE_EXCL` (Linux 6.6): like F7, but fail instead of reusing an existing superblock.
- F10: argument errors: a negative `fd` is `EINVAL` before anything else; a `key`/`value`/`aux` shape that does not fit the command is `EINVAL`; an unknown command is `EOPNOTSUPP`; a closed `fd` is `EBADF`; an `fd` that is not a filesystem context is `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The open-ended command interface mutates a kernel filesystem context, and its result depends on the filesystem driver's parser, on mount namespace and privilege for the create and reconfigure commands, and on the descriptor passed in. With the same context, driver and privileges it repeats.

Under Hermit the call returns `EPERM` without consulting the host, so it is deterministic by construction.

### Nondeterminism sources

- Filesystem-driver behaviour: which keys and values the parser accepts, and the messages it queues
- Mount namespace and privilege, for the create and reconfigure commands
- Detcore descriptor bookkeeping: the context descriptor is an object Detcore does not track

## Hermit design

Target policy: `refuse`. The legacy action was NEEDS-HANDLER; the code has since chosen refusal. What the code does at hermit f4055eb9cae4:

- `fsconfig` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 949) and is classified Determinized (`detcore/src/syscall_classification.rs:362`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2678) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run. `fsopen` and `fspick`, the only calls that create a filesystem context, are refused by the same arm, so a guest under Hermit cannot hold a context descriptor of its own making. Forwarding `fsconfig` would therefore never configure anything; on Linux such a call can only fail.

Where Hermit differs from Linux: since the guest has no context descriptor, native Linux would answer every `fsconfig` the guest can make with `EINVAL`, `EOPNOTSUPP` or `EBADF` (F10), never `EPERM` and never success. Hermit answers `EPERM`. The outcome, failure, is the same; only the errno differs. The host-strace case `fsconfig(-1, 0xffffffff, ...)` gives `EINVAL` natively and `EPERM` under Hermit.

Target (owner decision 6): keep the fixed `EPERM`, returned before any argument is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's own order, in `SYSCALL_DEFINE5(fsconfig)` in `fs/fsopen.c`, checks `fd`, the command and the `key`/`value`/`aux` shape before it looks up the descriptor, and checks privilege only inside the create and reconfigure commands (F10); Hermit deliberately does not reproduce it, because the call is refused, not emulated. A guest probing the new mount API with an invalid `fsconfig` sees `EPERM` where Linux gives `EINVAL`, and treats the API as unavailable: `fsopen` and `fspick`, the only sources of a context, return `EPERM` too.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fsconfig, ...)` with each `FSCONFIG_SET_*` command and well-shaped arguments, once on fd `-1` and once on an ordinary file descriptor. Each call returns -1 with `EPERM` (Linux gives `EINVAL` for both), and both runs agree.
- T2 (covers F7, F8, F9): `FSCONFIG_CMD_CREATE`, `FSCONFIG_CMD_RECONFIGURE` and `FSCONFIG_CMD_CREATE_EXCL` on an ordinary file descriptor return `EPERM`, and `/proc/self/mountinfo` read before and after is byte-identical.
- T3 (covers F10): the host-strace case `fsconfig(-1, 0xffffffff, -1, -1, -1)`, an unknown command on a valid descriptor, a shape mismatch (`FSCONFIG_SET_FLAG` with a non-NULL value), and a closed descriptor all return `EPERM` under Hermit where Linux returns `EINVAL`, `EOPNOTSUPP`, `EINVAL` and `EBADF`. The test pins the fixed errno of owner decision 6.
- T4 (covers none): record a guest that calls `fsconfig` and replay it; replay returns the same `EPERM` and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/359: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215-2219; fsopen and fspick are refused too, so natively every guest call would fail anyway, with EINVAL/EOPNOTSUPP/EBADF instead of EPERM); linux_determinism stays conditional; 10 flag behaviours and 4 tests listed; no runtime test names fsconfig, so tracking_status stays todo and ptrace_verification untested; open question on native argument errnos
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before any argument check stays the target; Linux order in SYSCALL_DEFINE5(fsconfig) deliberately not reproduced); target_policy stays refuse; T3 now pins the decision-6 errno; no new F/T ids; open question on native argument errnos removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fsconfig`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsconfig&body=Discussion+for+the+%60fsconfig%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsconfig.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsconfig%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fsconfig" (https://github.com/rrnewton/hermit/issues/359), legacy variant A2.

It was ranked 80 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### Open questions

- A context descriptor could still reach the guest from outside, for example through `SCM_RIGHTS` from a host process. Hermit would refuse `fsconfig` on it as well. This body does not treat that as a case to support.

### Note on the strace evidence below

The strace text below was written on 2026-07-24 against the host kernel, not through Hermit. At hermit f4055eb9cae4 Hermit never forwards `fsconfig`; the same call under Hermit gives `EPERM` (`detcore/src/lib.rs:2215-2219`), not `EINVAL`.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fsconfig -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(431, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fsconfig(-1, 0xffffffff /* FSCONFIG_??? */, 0xffffffffffffffff, 0xffffffffffffffff, -1) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
