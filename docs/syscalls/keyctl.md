---
schema: syscall-pages/v1
syscall: keyctl
nr_x86_64: 250
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/624
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+keyctl&body=Discussion+for+the+%60keyctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkeyctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60keyctl%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_kernel_keyring_syscall"
  refused: true
  refused_only_when_fail_closed: true
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`keyctl(operation, arg2, arg3, arg4, arg5)` is the multiplexed control interface of the kernel key-management facility: it reads, links, updates, revokes and searches keys and keyrings, manages session keyrings and request-key authority, and runs public-key and Diffie-Hellman operations. Under the default fail-closed policy Hermit returns `ENOSYS` for every operation without consulting the host, as if the kernel had been built without `CONFIG_KEYS` (`detcore/src/lib.rs:2125`). Only the explicit `--allow-unsupported-syscalls` opt-out forwards the call, which keeps rr's `keyctl` compatibility test working.

Manual reference: Linux man-pages keyctl(2), keyrings(7).

## Linux semantics

Keys and keyrings are kernel objects identified by serial numbers, which the kernel chooses at random when it creates a key (`key_alloc_serial`, `security/keys/key.c`). Special negative IDs (`KEY_SPEC_*`, `include/uapi/linux/keyctl.h`) name the calling thread's, process's and session's keyrings and the per-user keyrings. The session keyring is inherited across `fork` and `execve`, so a guest started from a login shell shares the host session's keyring.

Every operation checks a 32-bit permission mask with possessor, user, group and other fields, plus LSM policy. Objects are charged to per-user quotas (`/proc/sys/kernel/keys/`, visible in `/proc/key-users`). Key expiry uses wall-clock seconds, and revoked, invalidated and expired keys are removed by an asynchronous garbage collector.

The operation codes are defined in `include/uapi/linux/keyctl.h`. An unknown code gives `EOPNOTSUPP`.

### Flags and options

- F1: `KEYCTL_GET_KEYRING_ID` (0): map a special ID to the keyring's serial. A nonzero `arg3` creates the keyring if it does not exist yet. Returns a serial.
- F2: `KEYCTL_JOIN_SESSION_KEYRING` (1): with a `NULL` name, start a new anonymous session keyring; with a name, join the named keyring or create it. Returns the serial.
- F3: `KEYCTL_UPDATE` (2): replace a key's payload, if its type supports update.
- F4: `KEYCTL_REVOKE` (3): revoke a key; later accesses give `EKEYREVOKED`.
- F5: `KEYCTL_CHOWN` (4): change a key's uid and gid; -1 leaves a field unchanged. Changing the uid needs `CAP_SYS_ADMIN`.
- F6: `KEYCTL_SETPERM` (5): set the key's permission mask.
- F7: `KEYCTL_DESCRIBE` (6): write `type;uid;gid;perm;description` into a buffer and return the size needed.
- F8: `KEYCTL_CLEAR` (7): unlink every key from a keyring.
- F9: `KEYCTL_LINK` (8): link a key into a keyring, displacing a key of the same type and description.
- F10: `KEYCTL_UNLINK` (9): remove a key from a keyring.
- F11: `KEYCTL_SEARCH` (10): search a keyring tree for a key by type and description, optionally linking the result into a destination keyring. Returns the serial.
- F12: `KEYCTL_READ` (11): read a key's payload, or a keyring's list of member serials in link order, and return the size needed.
- F13: `KEYCTL_INSTANTIATE` (12): instantiate a key under construction; only a request-key handler that holds the authority may do this.
- F14: `KEYCTL_NEGATE` (13): negatively instantiate a key under construction for a timeout.
- F15: `KEYCTL_SET_REQKEY_KEYRING` (14): set the default request-key destination (`KEY_REQKEY_DEFL_*`) and return the old one.
- F16: `KEYCTL_SET_TIMEOUT` (15): set a key's expiry, in wall-clock seconds from now; 0 clears it.
- F17: `KEYCTL_ASSUME_AUTHORITY` (16): assume, or with 0 drop, the authority to instantiate a key under construction.
- F18: `KEYCTL_GET_SECURITY` (17): read the key's LSM security label.
- F19: `KEYCTL_SESSION_TO_PARENT` (18): replace the parent process's session keyring with the caller's, at the parent's next return to user space.
- F20: `KEYCTL_REJECT` (19): negatively instantiate a key under construction with a chosen error and timeout.
- F21: `KEYCTL_INSTANTIATE_IOV` (20): like F13 with the payload in an iovec array.
- F22: `KEYCTL_INVALIDATE` (21): mark a key invalid; the garbage collector removes it later.
- F23: `KEYCTL_GET_PERSISTENT` (22): get, creating if needed, a user's persistent keyring and link it into a keyring. It expires after a host-configured idle time.
- F24: `KEYCTL_DH_COMPUTE` (23): compute a Diffie-Hellman shared secret from keys holding the prime, base and private value, optionally through a KDF (`struct keyctl_kdf_params`).
- F25: `KEYCTL_PKEY_QUERY` (24): report an asymmetric key's supported operations and sizes (`struct keyctl_pkey_query`).
- F26: `KEYCTL_PKEY_ENCRYPT` (25): encrypt with an asymmetric key. Padding schemes with random padding, such as PKCS#1 v1.5, give a different ciphertext on every call.
- F27: `KEYCTL_PKEY_DECRYPT` (26): decrypt with an asymmetric key.
- F28: `KEYCTL_PKEY_SIGN` (27): sign with an asymmetric key.
- F29: `KEYCTL_PKEY_VERIFY` (28): verify a signature with an asymmetric key.
- F30: `KEYCTL_RESTRICT_KEYRING` (29): restrict which keys may be linked into a keyring.
- F31: `KEYCTL_MOVE` (30): move a key from one keyring to another; `KEYCTL_MOVE_EXCL` fails instead of displacing a matching key in the target.
- F32: `KEYCTL_CAPABILITIES` (31): copy the keyring subsystem's capability bits (`KEYCTL_CAPS0_*`, `KEYCTL_CAPS1_*`) into a buffer; the bits depend on the host kernel's configuration.
- F33: `KEYCTL_WATCH_KEY` (32): attach a key or keyring to a watch-queue pipe, so changes produce notifications.
- F34: an unknown operation code: `EOPNOTSUPP`.
- F35: common errors across operations: `ENOKEY`, `EKEYREVOKED`, `EKEYEXPIRED`, `EACCES`, `EDQUOT`, `EFAULT`, `EINVAL` and `ENOTDIR` for a key that should be a keyring.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Every operation that returns or reports a serial (F1, F2, F11, F12) exposes random numbers, and most operations act on host keyrings that the guest inherits from the process that started Hermit. Some outputs are random by design (F26) or depend on wall-clock time (F16, F23) and on the host kernel's configuration (F32). A failing call on an object that cannot exist, such as an unknown operation code, repeats, but the main uses of the call do not.

### Nondeterminism sources

- Random key serial numbers
- Host session, user and persistent keyring contents, inherited by the guest
- Wall-clock key expiry and asynchronous garbage collection
- Randomized public-key padding (F26)
- Host kernel configuration, LSM labels and quotas
- Concurrent mutation by processes outside the container, and changes made to the parent process's session (F19)

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_KEYS`, returned before any argument is examined, so a valid call and a garbage-argument call get the same `ENOSYS`, for every operation code including an unknown one. No unprivileged success case needs preserving: a kernel without `CONFIG_KEYS` refuses every caller, so decision 6's exceptions do not apply. Owner decision 9 makes this the long-term target; Detcore will not model a guest-private keyring. The supportable fallback is a program that treats `ENOSYS` as a keyring-less kernel and keeps its secrets elsewhere; the in-tree workload that needs a working keyring, rr's `keyctl` compatibility test (comment at `detcore/src/lib.rs:2121`), runs only under the `--allow-unsupported-syscalls` opt-out. Today's code realizes the target under the default fail-closed policy (`detcore/src/lib.rs:2125-2126`). The opt-out forwards the call to the host (`detcore/src/lib.rs:2129`); that is a reduced-fidelity mode the user asks for explicitly, not the target, so no code change is needed.

- `keyctl` is classified Determinized (`detcore/src/syscall_classification.rs:478`) and belongs to the kernel-keyring family predicate `is_kernel_keyring_syscall` (`detcore/src/syscall_classification.rs:1243`). The predicate's doc comment (`detcore/src/syscall_classification.rs:1238`) says keyring serials, quotas, permissions and contents are shared host-kernel state, and that a fixed `ENOSYS` lets feature probes take their fallback and keeps the guest from reading or mutating host keyrings.
- The dispatch arm (`detcore/src/lib.rs:2125`) returns `Err(Errno::ENOSYS)` when `panic_on_unsupported_syscalls` is set (`detcore/src/lib.rs:2126`), without reading the operation code. All of F1-F35 give `ENOSYS`, including the unknown-operation case F34, where Linux would give `EOPNOTSUPP`. That matches a kernel without `CONFIG_KEYS`. The run is not stopped.
- The flag's documentation (`detcore-model/src/config.rs:447`) says ordinary `hermit run` enables this policy and that compatibility requires `--allow-unsupported-syscalls`. Under that opt-out the call is forwarded to the host (`detcore/src/lib.rs:2129`). The arm's comment (`detcore/src/lib.rs:2121`) says this keeps the guest on a real working keyring for the rr `keyctl` compatibility test. `kernel_keyring_passes_through_with_compatibility_opt_out` (`hermit-cli/tests/kernel_keyring.rs:108`) checks that the opt-out reproduces the native ENOSYS-or-not verdict.
- The family is in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), which backends that bypass Detcore's dispatcher consult to enforce the same refusal, and the performance opt-in subscription still subscribes it (`detcore/src/lib.rs:1256`).

Why `refuse` and not the alternatives:

- `passthrough` exposes random serials and host keyring contents, lets the guest change the host's session keyring and its parent's (F2, F19), and gives randomized and time-dependent results (F16, F26).
- `model` or `emulate` would need a guest-private key store covering 33 operations, deterministic serials, permission and quota checks, request-key authority, and the crypto operations. Detcore has none of that. The legacy NEEDS-HANDLER recommendation predates the refusal (https://github.com/rrnewton/hermit/pull/848 per the code tags).
- Without `CONFIG_KEYS` a real kernel returns `ENOSYS`, so keyutils and its callers already handle it.

## Recommended tests

- T1 (covers F1, F35): under default-policy `hermit run --verify` on ptrace, `keyctl(KEYCTL_GET_KEYRING_ID, KEY_SPEC_SESSION_KEYRING, 0)` returns -1 with `ENOSYS`, and both runs match.
  Existing candidate: `tests/c/keyctl_enosys.c`, run by the e2e cell `c-programs/keyctl-enosys` (`tests/e2e/manifests/c-programs.yaml:2867`, verify ci on ptrace) and by `kernel_keyring_is_deterministically_unavailable` at `hermit-cli/tests/kernel_keyring.rs:34` with `--backend=ptrace run --strict --verify --panic-on-unsupported-syscalls`.
- T2 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24, F25, F26, F27, F28, F29, F30, F31, F32, F33, F34, F35): under the same default policy, issue every operation code 0 through 32 and an unknown code (for example 1000) with inert arguments: special IDs, a bogus serial, `NULL` buffers. Every call must return -1 with `ENOSYS`, and both runs must match.
- T3 (covers F2, F9, F19, F23): host isolation. Read the host's `keyctl show @s` and the `/proc/key-users` line for the test's uid before and after a guest that tries `KEYCTL_JOIN_SESSION_KEYRING`, `KEYCTL_LINK`, `KEYCTL_SESSION_TO_PARENT` and `KEYCTL_GET_PERSISTENT` under the default policy. Nothing on the host may change, including the session keyring of the shell that started Hermit.
- T4 (covers F1): compatibility opt-out. The guest's ENOSYS-or-not verdict for `KEYCTL_GET_KEYRING_ID` is the same natively and under `hermit run --allow-unsupported-syscalls` on ptrace.
  Existing candidate: `kernel_keyring_passes_through_with_compatibility_opt_out` at `hermit-cli/tests/kernel_keyring.rs:108`, with guest `tests/c/keyctl_passthrough.c`. The same guest also runs in the e2e cell `c-programs/keyctl-passthrough` (`tests/e2e/manifests/c-programs.yaml:2931`, verify ci on ptrace), which checks only that two runs print the same line.
- T5 (covers F1, F12): record and replay a guest that calls `keyctl` under the default policy. Both the recording and the replay must return `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/624: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_kernel_keyring_syscall, refused only when fail-closed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (default policy returns a fixed ENOSYS for every operation, the CONFIG_KEYS-absent boundary, at detcore/src/lib.rs:2125; host passthrough only under --allow-unsupported-syscalls for the rr keyctl compatibility test); linux_determinism stays nondeterministic; 35 flag behaviours (33 operation codes, unknown code, common errors) and 5 tests listed; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_KEYS, before any argument check, realized under the default policy at detcore/src/lib.rs:2125-2126; the opt-out passthrough at detcore/src/lib.rs:2129 stays a user-chosen reduced-fidelity mode); target_policy stays refuse; no new F/T ids; removed the open question on refuse versus a guest-private keyring model
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/keyctl_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `keyctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+keyctl&body=Discussion+for+the+%60keyctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkeyctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60keyctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: keyctl" (https://github.com/rrnewton/hermit/issues/624), legacy variant B.

### Related tests

- `hermit-cli/tests/key_users_determinism.rs` names `SYS_keyctl` (`hermit-cli/tests/key_users_determinism.rs:80`, `KEYCTL_UPDATE`, and `hermit-cli/tests/key_users_determinism.rs:119`, `KEYCTL_UNLINK`), but those calls run natively in the test process, not in a guest. They churn a host key's payload so that `/proc/key-users` changes while `cat`, `awk` and `sed` read it under strict verification. The test covers `/proc/key-users` virtualization, not this syscall under Hermit.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
