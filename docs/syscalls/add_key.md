---
schema: syscall-pages/v1
syscall: add_key
nr_x86_64: 248
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/332
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+add_key&body=Discussion+for+the+%60add_key%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fadd_key.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60add_key%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_kernel_keyring_syscall"
  refused: true
  refused_only_when_fail_closed: true
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`add_key(type, description, payload, plen, keyring)` creates a key in a kernel keyring, or updates a matching key that is already there, and returns the key's serial number. Under the default fail-closed policy Hermit returns `ENOSYS` without consulting the host, as if the kernel had been built without `CONFIG_KEYS` (`detcore/src/lib.rs:2125`). Only the explicit `--allow-unsupported-syscalls` opt-out forwards the call to the host kernel.

Manual reference: Linux man-pages add_key(2), keyrings(7).

## Linux semantics

The kernel looks up the key type by name, resolves the destination keyring (a serial or a special `KEY_SPEC_*` ID, `include/uapi/linux/keyctl.h`), checks write permission on it, and lets the type parse the payload. If the destination already holds a key with the same type and description and the type supports update, that key is updated in place and its existing serial is returned. Otherwise a new key is allocated, linked into the destination, and any old key with the same type and description is unlinked.

Every new key is charged to the owner's quota in its user namespace (`/proc/sys/kernel/keys/maxkeys` and `maxbytes`, visible in `/proc/key-users`). A new key's serial number is chosen at random: `key_alloc_serial` in `security/keys/key.c` draws it with `get_random_bytes` and retries until it finds an unused positive value of 3 or more.

The thread, process and session keyrings named by the special IDs can be created on demand. The session keyring is inherited across `fork` and `execve`, so a process started from a login shell shares the host session's keyring.

### Flags and options

- F1: `type` selects the key type and its payload parser: `user`, `logon`, `keyring`, `big_key`, `asymmetric` and others the kernel configuration provides. An unknown type gives `ENODEV`. A type name beginning with `.` is reserved for the kernel and gives `EPERM`. A type string longer than 32 bytes, including the terminator, gives `EINVAL`.
- F2: `description`: a string of at most 4096 bytes, including the terminator, or `EINVAL`. For the `logon` type it must start with a `service:` prefix, or the call fails with `EINVAL`.
- F3: `payload` and `plen`: the payload the type parses. `plen` above 1 MiB minus 1 gives `EINVAL`. A payload the type rejects gives `EINVAL`. The `keyring` type takes no payload. A bad pointer gives `EFAULT`.
- F4: `keyring` as a special ID: `KEY_SPEC_THREAD_KEYRING` (-1), `KEY_SPEC_PROCESS_KEYRING` (-2) and `KEY_SPEC_SESSION_KEYRING` (-3) are created on demand. `KEY_SPEC_USER_KEYRING` (-4), `KEY_SPEC_USER_SESSION_KEYRING` (-5), `KEY_SPEC_GROUP_KEYRING` (-6) and `KEY_SPEC_REQUESTOR_KEYRING` (-8) name the per-user, per-group and request-key destination keyrings.
- F5: `keyring` as a serial number: a keyring the caller can write. A key that is not a keyring gives `ENOTDIR`. A missing keyring gives `ENOKEY`, a revoked one `EKEYREVOKED`, an expired one `EKEYEXPIRED`, and one the caller may not write `EACCES`.
- F6: update of an existing key: if the destination holds a key with the same type and description and the type supports update, the payload replaces the old one and the old serial is returned. The `keyring` type does not support update, so adding a keyring always creates a new one and displaces the old link.
- F7: creation of a new key: the call returns a fresh, randomly chosen serial and charges the owner's key quota. Exceeding the quota gives `EDQUOT`.

## Determinism analysis

`linux_determinism: nondeterministic` (was `conditional` in the rendered body). Every successful call returns a serial number, and every new serial is random (F7). An update (F6) returns the serial of a key that was created earlier, which was random too. Only error results repeat: an unknown type, an oversized `plen`, or a missing destination gives the same errno on every run, as the host strace under Additional information shows for an all-ones argument set. The success path also depends on host state that Hermit does not control.

### Nondeterminism sources

- Random key serial numbers (`key_alloc_serial`, `security/keys/key.c`)
- Host session, user and persistent keyring contents, inherited by the guest from the process that started Hermit
- Quotas, permissions and LSM policy
- Key expiry, revocation and garbage collection, and concurrent mutation by processes outside the container
- Which key types the host kernel provides (F1)

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_KEYS`, returned before any argument is examined, so a valid call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving: a kernel without `CONFIG_KEYS` refuses every caller, so decision 6's exceptions do not apply. Owner decision 9 makes this the long-term target; Detcore will not model a guest-private keyring. The supportable fallback is a program that treats `ENOSYS` as a keyring-less kernel and keeps its secrets elsewhere; the in-tree workload that needs a working keyring, rr's `keyctl` compatibility test (comment at `detcore/src/lib.rs:2121`), runs only under the `--allow-unsupported-syscalls` opt-out. Today's code realizes the target under the default fail-closed policy (`detcore/src/lib.rs:2125-2126`). The opt-out forwards the call to the host (`detcore/src/lib.rs:2129`); that is a reduced-fidelity mode the user asks for explicitly, not the target, so no code change is needed.

- `add_key` is classified Determinized (`detcore/src/syscall_classification.rs:474`) and belongs to the kernel-keyring family predicate `is_kernel_keyring_syscall` (`detcore/src/syscall_classification.rs:1243`). The predicate's doc comment (`detcore/src/syscall_classification.rs:1238`) states the reason: key serials, quotas, permissions, contents and request-key upcalls are shared host-kernel state, and a fixed `ENOSYS` matches a kernel without `CONFIG_KEYS`, so feature probes take their established fallback.
- The dispatch arm (`detcore/src/lib.rs:2125`) returns `Err(Errno::ENOSYS)` when `panic_on_unsupported_syscalls` is set (`detcore/src/lib.rs:2126`). It never reads the arguments, so every flag F1-F7 gets the same `ENOSYS`, including inputs that would give `EFAULT` or `EINVAL` on Linux. The run is not stopped; the guest just sees the errno.
- The flag's documentation (`detcore-model/src/config.rs:447`) says ordinary `hermit run` enables this policy and that compatibility requires the explicit `--allow-unsupported-syscalls` opt-out. Under that opt-out the arm forwards the call to the host (`detcore/src/lib.rs:2129`), so the guest sees real host keyrings, random serials and host side effects. That mode is not deterministic and is kept only so programs that need a working keyring can run.
- The family is in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), and through it in `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`). Backends that execute syscalls outside Detcore's dispatcher consult that predicate to enforce the same refusal (comment at `detcore/src/syscall_classification.rs:2714`).
- The performance opt-in subscription, where unlisted syscalls bypass Detcore, still subscribes all three keyring syscalls (`detcore/src/lib.rs:1256`), so the refusal holds there too.

Why `refuse` and not the alternatives:

- `passthrough` leaks random serials and host keyring contents into the guest, and lets the guest mutate the host's session and user keyrings.
- `model` or `emulate` would need a guest-private keyring store with deterministic serials, quota accounting, permission checks, every key type's payload parser, and request-key upcalls. No such model exists in Detcore, and the legacy NEEDS-HANDLER recommendation predates the refusal (https://github.com/rrnewton/hermit/pull/848 per the code tags).
- An `ENOSYS` refusal is a state real kernels exhibit (`CONFIG_KEYS=n`), so well-behaved callers already handle it.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7): under default-policy `hermit run --verify` on ptrace, issue raw `syscall(SYS_add_key, ...)` with one argument set per flag: a `user` key into the process keyring, a `keyring` type, a `logon` description without a `service:` prefix, a `.`-prefixed type, an unknown type, `plen` above 1 MiB, a bad `payload` pointer, each special keyring ID, a non-keyring serial, and the same type and description twice. Every call must return -1 with `ENOSYS`, and both runs must match.
  Existing partial candidate: `tests/c/add_key_enosys.c` (one argument set: a `user` key into `KEY_SPEC_PROCESS_KEYRING`), run by the e2e cell `c-programs/add-key-enosys` (`tests/e2e/manifests/c-programs.yaml:192`, verify ci on ptrace) and by `kernel_keyring_is_deterministically_unavailable` at `hermit-cli/tests/kernel_keyring.rs:34` with `--backend=ptrace run --strict --verify --panic-on-unsupported-syscalls`.
- T2 (covers F4, F6, F7): host isolation. Read the host's `/proc/key-users` line for the test's uid and `keyctl show @s` before and after a guest that calls `add_key` into its session and user keyrings under the default policy. The host keyrings and key counts must be unchanged.
- T3 (covers F1, F7): compatibility opt-out. Run an `add_key` guest natively and under `hermit run --allow-unsupported-syscalls` on ptrace, and compare only the ENOSYS-or-not verdict, as `kernel_keyring_passes_through_with_compatibility_opt_out` at `hermit-cli/tests/kernel_keyring.rs:108` does for `keyctl`. The serial itself is random and must not be compared. No `add_key` variant of that test exists yet.
- T4 (covers F1, F7): record and replay a guest that calls `add_key` under the default policy. Both the recording and the replay must return `ENOSYS`, and replay must not reach the host keyring.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/332#issuecomment-5872933124
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/332: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_kernel_keyring_syscall, refused only when fail-closed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; add_key records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/332 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (default policy returns a fixed ENOSYS, the CONFIG_KEYS-absent boundary, at detcore/src/lib.rs:2125; host passthrough only under --allow-unsupported-syscalls); linux_determinism conditional -> nondeterministic (every success returns a randomly allocated serial); 7 flag behaviours and 4 tests listed; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_KEYS, before any argument check, realized under the default policy at detcore/src/lib.rs:2125-2126; the opt-out passthrough at detcore/src/lib.rs:2129 stays a user-chosen reduced-fidelity mode); target_policy stays refuse; no new F/T ids; removed the open question on refuse versus a guest-private keyring model

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `add_key`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+add_key&body=Discussion+for+the+%60add_key%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fadd_key.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60add_key%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: add_key" (https://github.com/rrnewton/hermit/issues/332), legacy variant A2. It was ranked 53 in the batch-2 classification sequence, which excluded the 50 workload-observed batch-1 syscalls.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/332.

### Related tests

- `hermit-cli/tests/key_users_determinism.rs` names `SYS_add_key` (`hermit-cli/tests/key_users_determinism.rs:54`), but the call runs natively in the test process, not in a guest. It creates a host key and churns its payload so that `/proc/key-users` changes, then checks that `cat`, `awk` and `sed` reading that file pass strict verification. It tests `/proc/key-users` virtualization and does not cover this syscall under Hermit.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=add_key -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(248, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
add_key(0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 18446744073709551615, KEY_SPEC_THREAD_KEYRING) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

The `EINVAL`, rather than `EFAULT` for the invalid `type` pointer, is consistent with the kernel checking `plen` (here 2^64-1) before it copies the type string. That ordering is an inference from the result, not read from the kernel source.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/332#issuecomment-5872933124
