---
schema: syscall-pages/v1
syscall: request_key
nr_x86_64: 249
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/403
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+request_key&body=Discussion+for+the+%60request_key%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frequest_key.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60request_key%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_kernel_keyring_syscall"
  refused: true
  refused_only_when_fail_closed: true
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`request_key(type, description, callout_info, dest_keyring)` searches the caller's keyrings for a key and returns its serial number. If no key is found and `callout_info` is given, the kernel runs the user-space `/sbin/request-key` helper to construct one. Under the default fail-closed policy Hermit returns `ENOSYS` without consulting the host, as if the kernel had been built without `CONFIG_KEYS` (`detcore/src/lib.rs:2125`). Only the explicit `--allow-unsupported-syscalls` opt-out forwards the call.

Manual reference: Linux man-pages request_key(2), keyrings(7), request-key(8).

## Linux semantics

The kernel searches the thread, process and session keyrings in that order. Through the session keyring, or the user-session keyring when there is none, it also reaches the user's keyrings. It returns the first matching key the caller may search. The call needs `CONFIG_KEYS`; without it the syscall gives `ENOSYS`.

If nothing matches and `callout_info` is not `NULL`, the kernel creates an uninstantiated key, starts `/sbin/request-key` through the usermode helper, and waits. The helper, configured by the host's `/etc/request-key.conf` and `/etc/request-key.d/`, instantiates, negates or rejects the key with `keyctl`. The caller sleeps until it does, and a signal can interrupt the wait.

A found or constructed key is linked into the destination keyring when one is given. A key's serial number was chosen at random when it was created (`key_alloc_serial`, `security/keys/key.c`).

Verified on the host (strace under Additional information): a lookup of a nonexistent `user` key with no callout fails with `ENOKEY`.

### Flags and options

- F1: `type` and `description`: a type the kernel does not provide gives `ENOKEY`. A type name beginning with `.` gives `EPERM`. Oversized or unreadable strings give `EINVAL` or `EFAULT`.
- F2: search-only lookup (`callout_info == NULL`): search the thread, process and session keyrings and the user keyrings reached through them, and return the first match. With no match the call gives `ENOKEY`.
- F3: lookup with upcall (`callout_info` not `NULL`): if no key matches, construct one through `/sbin/request-key` and wait for it. The outcome depends on the host's request-key configuration and helper programs.
- F4: `dest_keyring == 0`: link the result into the default request-key keyring, chosen by `KEYCTL_SET_REQKEY_KEYRING` and falling back through the thread, process and session keyrings.
- F5: `dest_keyring` as a special `KEY_SPEC_*` ID or a serial: link the result into that keyring, which needs write permission, or `EACCES`.
- F6: a key found negative, rejected, revoked or expired: `ENOKEY`, `EKEYREJECTED`, `EKEYREVOKED` or `EKEYEXPIRED`, which a later call repeats only until the negative key times out.
- F7: a signal interrupts the wait for construction: `EINTR`.
- F8: quota exhausted while constructing a key: `EDQUOT`.

## Determinism analysis

`linux_determinism: nondeterministic` (was `conditional` in the rendered body). A successful call returns the serial of a key that the host or a request-key helper created, and every key serial is random. Which key is found, if any, depends on the host's session and user keyrings, which the guest inherits from the process that started Hermit. With an upcall (F3) the result also depends on host configuration files, helper programs and scheduling. The lookup that cannot match, such as the host probe's `ENOKEY` for a nonexistent `user` key with no callout, repeats only while no process on the host creates such a key.

### Nondeterminism sources

- Random key serial numbers
- Host session, user and persistent keyring contents, which differ across machines and login sessions
- `/sbin/request-key` upcalls: host configuration, helper programs and their timing
- Signal timing during the construction wait (F7)
- Negative-key timeouts, key expiry and concurrent mutation by processes outside the container

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_KEYS`, returned before any argument is examined, so a valid call and a garbage-argument call get the same `ENOSYS`, and no request-key upcall ever starts. No unprivileged success case needs preserving: a kernel without `CONFIG_KEYS` refuses every caller, so decision 6's exceptions do not apply. Owner decision 9 makes this the long-term target; Detcore will not model a guest-private keyring. The supportable fallback is a program that treats `ENOSYS` as a keyring-less kernel and keeps its secrets elsewhere; the in-tree workload that needs a working keyring, rr's `keyctl` compatibility test (comment at `detcore/src/lib.rs:2121`), runs only under the `--allow-unsupported-syscalls` opt-out. Today's code realizes the target under the default fail-closed policy (`detcore/src/lib.rs:2125-2126`). The opt-out forwards the call to the host (`detcore/src/lib.rs:2129`); that is a reduced-fidelity mode the user asks for explicitly, not the target, so no code change is needed.

- `request_key` is classified Determinized (`detcore/src/syscall_classification.rs:476`) and belongs to the kernel-keyring family predicate `is_kernel_keyring_syscall` (`detcore/src/syscall_classification.rs:1243`). The predicate's doc comment (`detcore/src/syscall_classification.rs:1238`) names `request_key` user-space upcalls as part of the shared host-kernel state the family exposes, and says a fixed `ENOSYS` keeps the guest from triggering host keyrings.
- The dispatch arm (`detcore/src/lib.rs:2125`) returns `Err(Errno::ENOSYS)` when `panic_on_unsupported_syscalls` is set (`detcore/src/lib.rs:2126`), without reading any argument, so F1-F8 all give `ENOSYS` and no upcall ever starts. The run is not stopped.
- The flag's documentation (`detcore-model/src/config.rs:447`) says ordinary `hermit run` enables this policy and that compatibility requires `--allow-unsupported-syscalls`. Under that opt-out the call is forwarded to the host (`detcore/src/lib.rs:2129`). A forwarded call with `callout_info` can then start a host helper process outside the container and outside Detcore's scheduler, which the default policy rules out.
- The family is in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), which backends that bypass Detcore's dispatcher consult to enforce the same refusal, and the performance opt-in subscription still subscribes it (`detcore/src/lib.rs:1256`).

Why `refuse` and not the alternatives:

- `passthrough` returns host keys and random serials, and an upcall runs a host program that the guest triggered.
- `model` would need a guest-private keyring store and a deterministic replacement for the request-key helper. Detcore has neither. The legacy NEEDS-INVESTIGATION recommendation predates the refusal (https://github.com/rrnewton/hermit/pull/848 per the code tags).
- `ENOSYS` is what a kernel without `CONFIG_KEYS` returns, so callers such as keyutils, NFS id mapping and Kerberos keyring caches already have a fallback for it.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8): under default-policy `hermit run --verify` on ptrace, issue raw `syscall(SYS_request_key, ...)` with a nonexistent `user` key and no callout, the same key with a non-`NULL` `callout_info`, a `.`-prefixed type, a bad `description` pointer, `dest_keyring` 0, each special ID and a bogus serial. Every call must return -1 with `ENOSYS`, and both runs must match.
  Existing partial candidate: `tests/c/request_key_enosys.c` (one argument set: a missing `user` key, no callout, `KEY_SPEC_PROCESS_KEYRING`), run by the e2e cell `c-programs/request-key-enosys` (`tests/e2e/manifests/c-programs.yaml:7482`, verify ci on ptrace) and by `kernel_keyring_is_deterministically_unavailable` at `hermit-cli/tests/kernel_keyring.rs:34`.
- T2 (covers F3): no upcall escapes. On a host with `/sbin/request-key` installed, run a guest that calls `request_key` with a `callout_info` under the default policy, and check that no `request-key` helper process starts and the host keyrings and `/proc/key-users` for the uid are unchanged.
- T3 (covers F2, F6): compatibility opt-out. Run a `request_key` guest that looks up a nonexistent `user` key natively and under `hermit run --allow-unsupported-syscalls` on ptrace, and compare the errno. `kernel_keyring_passes_through_with_compatibility_opt_out` at `hermit-cli/tests/kernel_keyring.rs:108` is the pattern for `keyctl`; no `request_key` variant exists yet.
- T4 (covers F2): record and replay a guest that calls `request_key` under the default policy. Both the recording and the replay must return `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/403: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_kernel_keyring_syscall, refused only when fail-closed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (default policy returns a fixed ENOSYS, the CONFIG_KEYS-absent boundary, at detcore/src/lib.rs:2125, so no host keyring lookup or request-key upcall happens; host passthrough only under --allow-unsupported-syscalls); linux_determinism conditional -> nondeterministic (a success returns a random serial from host keyrings); 8 flag behaviours and 4 tests listed; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_KEYS, before any argument check, realized under the default policy at detcore/src/lib.rs:2125-2126; the opt-out passthrough at detcore/src/lib.rs:2129 stays a user-chosen reduced-fidelity mode); target_policy stays refuse; no new F/T ids; removed the open question on refuse versus a guest-private keyring model

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `request_key`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+request_key&body=Discussion+for+the+%60request_key%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frequest_key.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60request_key%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: request_key" (https://github.com/rrnewton/hermit/issues/403), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=request_key` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `request_key` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
request_key("user", "nonexistent-desc", NULL, 0) = -1 ENOKEY (Required key not available)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

The "UNCLASSIFIED temporary-passthrough" sentence above describes the code on 2026-07-24. At hermit f4055eb9cae4 `request_key` is classified Determinized and refused with `ENOSYS` under the default policy, as described under Hermit design.
