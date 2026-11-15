---
schema: syscall-pages/v1
syscall: landlock_create_ruleset
nr_x86_64: 444
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/627
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+landlock_create_ruleset&body=Discussion+for+the+%60landlock_create_ruleset%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flandlock_create_ruleset.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60landlock_create_ruleset%60%2C+or+in+what+the+page+says%3F%0A
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

`landlock_create_ruleset(attr, size, flags)` creates a Landlock ruleset and returns a file descriptor for it, or, with a query flag, reports the kernel's Landlock ABI version or errata. Detcore never forwards it to the host: at hermit f4055eb9cae4 it returns a fixed `ENOSYS` (`detcore/src/lib.rs:2322-2324`), which is what a kernel built without Landlock answers, so a guest always sees "Landlock unavailable".

Manual reference: Linux man-pages landlock_create_ruleset(2); UAPI header `linux/landlock.h`.

## Linux semantics

Landlock is an unprivileged, stackable LSM. A process builds a ruleset (this call), adds rules to it (`landlock_add_rule`), and enforces it on itself (`landlock_restrict_self`). glibc has no wrapper; programs call it through `syscall(2)` or a library such as the `landlock` Rust crate.

Availability is decided by the host kernel, before any argument is examined:

- built without `CONFIG_SECURITY_LANDLOCK`: the syscall is not implemented and returns `ENOSYS`;
- built with Landlock but Landlock not in the active LSM list (`lsm=` boot parameter or the build default): `EOPNOTSUPP`;
- otherwise the call proceeds, and the set of access bits the kernel knows depends on its Landlock ABI version (roughly: ABI 1 in Linux 5.13, 2 adds `REFER`, 3 adds `TRUNCATE`, 4 adds TCP network rights, 5 adds `IOCTL_DEV`, 6 adds scopes, 7 adds the logging flags and the errata query).

With `flags == 0`, `attr` points to a `struct landlock_ruleset_attr` of `size` bytes, read as an extensible struct: `size` smaller than the first field (`handled_access_fs`, 8 bytes) is `EINVAL`, a struct larger than the kernel knows must have zero trailing bytes or the call fails with `E2BIG`, and a bad pointer is `EFAULT`. The new descriptor is an anonymous inode (`[landlock-ruleset]`) opened read-write with close-on-exec, at the lowest free descriptor number.

### Flags and options

- F1: `flags == 0`, ruleset creation: copies `struct landlock_ruleset_attr` (extensible-struct rules: `size` below 8 gives `EINVAL`, unknown non-zero trailing bytes or a size above one page give `E2BIG`, a bad pointer gives `EFAULT`) and returns a new close-on-exec ruleset descriptor.
- F2: `handled_access_fs` bitmask: the `LANDLOCK_ACCESS_FS_*` rights (`EXECUTE`, `WRITE_FILE`, `READ_FILE`, `READ_DIR`, `REMOVE_DIR`, `REMOVE_FILE`, `MAKE_CHAR`, `MAKE_DIR`, `MAKE_REG`, `MAKE_SOCK`, `MAKE_FIFO`, `MAKE_BLOCK`, `MAKE_SYM`, `REFER`, `TRUNCATE`, `IOCTL_DEV`) that the ruleset denies by default. A bit the running kernel does not know gives `EINVAL`. For historical reasons `REFER` is denied by default even when its bit is not set (UAPI header comment); a rule can grant it only if the bit is set here.
- F3: `handled_access_net` bitmask: `LANDLOCK_ACCESS_NET_BIND_TCP` and `LANDLOCK_ACCESS_NET_CONNECT_TCP` (ABI 4). Unknown bits give `EINVAL`.
- F4: `scoped` bitmask: `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` and `LANDLOCK_SCOPE_SIGNAL` (ABI 6), which stop the domain from connecting to abstract UNIX sockets or signalling processes outside it. Unknown bits give `EINVAL`.
- F5: all three masks zero: `ENOMSG` (a ruleset that handles nothing is refused).
- F6: `LANDLOCK_CREATE_RULESET_VERSION` with `attr == NULL` and `size == 0`: returns the highest supported ABI version (1 or more) and creates nothing. This is the canonical availability probe.
- F7: `LANDLOCK_CREATE_RULESET_ERRATA` with `attr == NULL` and `size == 0`: returns a bitmask of fixed issues for the running ABI version (recent kernels only; older ones give `EINVAL`).
- F8: any other `flags` value, both query flags at once, or a query flag with a non-NULL `attr` or non-zero `size`: `EINVAL`.
- F9: availability: `ENOSYS` on a kernel without Landlock, `EOPNOTSUPP` when Landlock is built but not enabled at boot. Both are returned before any argument check.
- F10: resource failure while creating the descriptor: `EMFILE` (descriptor table full) or `ENOMEM`.

## Determinism analysis

`linux_determinism: conditional`. On one host with a fixed kernel build, boot LSM list and descriptor table, the call is deterministic: the version, errata and error results are constants of the kernel, and the new descriptor takes the lowest free number. It does not repeat across hosts:

- whether Landlock exists (`ENOSYS`) or is enabled (`EOPNOTSUPP`) is a property of the kernel build and boot command line. Measured on the measurement host on 2026-10-03 (kernel 7.1.3, `/boot/config-7.1.3-...` says `# CONFIG_SECURITY_LANDLOCK is not set`, `/sys/kernel/security/lsm` reads `capability,bpf,ima`): a raw `syscall(444, NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` and the `ERRATA` probe both returned `-1`/`ENOSYS` natively;
- the ABI version (F6) and errata bitmask (F7) differ between kernel releases, and the same mask (F2-F4) is valid on one kernel and `EINVAL` on an older one;
- programs branch on these answers (best-effort sandboxing picks the rights to handle from the reported ABI), so the rest of the run diverges with them.

Under Hermit the answer is a constant `ENOSYS`, so these host differences are removed.

## Hermit design

Target policy: `refuse`, which is what the code does at hermit f4055eb9cae4.

- `classify_syscall` lists `landlock_create_ruleset` as Determinized (`detcore/src/syscall_classification.rs:464`), and `is_landlock_sandbox_syscall` selects the three Landlock syscalls (`detcore/src/syscall_classification.rs:1121-1125`).
- In `handle_syscall_event`, the arm at `detcore/src/lib.rs:2322-2324` returns `Err(Error::Errno(Errno::ENOSYS))` before the typed match. It does not read `panic_on_unsupported_syscalls` (loaded at `detcore/src/lib.rs:1915`), so the answer is the same with and without `--strict`, and nothing reaches the host: no ruleset is created and no descriptor number is consumed.
- `is_deterministically_refused_syscall` includes the family (`detcore/src/syscall_classification.rs:1327`). Per the comment at `detcore/src/syscall_classification.rs:2714-2717`, backends that execute guest syscalls outside Detcore's dispatcher consult that predicate to apply the same fixed refusal.
- `ENOSYS` is checked before any argument, as on a kernel without Landlock, so malformed calls (F1, F5, F8) also get `ENOSYS` rather than `EINVAL`, `E2BIG`, `EFAULT` or `ENOMSG`. Library code written for best-effort sandboxing (for example the `landlock` crate's best-effort mode) treats `ENOSYS` as "not supported" and continues unsandboxed.

Why not the alternatives:

- `passthrough` makes the result a property of the host (F9, F6, F7), and a created ruleset would later let the guest install a real host LSM domain that Detcore cannot see (see landlock_restrict_self).
- `emulate` with a fake success (a descriptor and a fixed ABI version) would tell the guest it is sandboxed when no restriction is enforced; that is a security lie, not a compatibility shim.
- `model` would mean Detcore enforcing Landlock rules itself on every path, port, signal and socket operation. Nothing in the code plans this, and no workload so far needs it.

Cost of `refuse`: a program that requires Landlock (not best effort) exits or refuses to run under Hermit. That failure is deterministic.

## Recommended tests

- T1 (covers F6, F9): under `hermit run --verify` on ptrace, a guest probes `syscall(SYS_landlock_create_ruleset, NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` and must see `-1` with `errno == ENOSYS`, identically in both runs and on a host whose kernel does support Landlock.
  Existing guest: `tests/rust/landlock.rs:43-52` (cargo bin `rustbin_landlock`, `tests/Cargo.toml:86-89`). At this SHA no cargo integration test and no e2e manifest program runs it; it is only picked up by the internal Buck glob at `tests/BUCK:415`. It needs a ptrace runner before it can be recorded as coverage.
- T2 (covers F7, F8): `LANDLOCK_CREATE_RULESET_ERRATA` with NULL/0, `flags == 3`, `flags == 0x80`, and `VERSION` with a non-NULL `attr` all return `ENOSYS`, not a version, a bitmask or `EINVAL`.
- T3 (covers F1, F2, F3, F4, F5, F10): `flags == 0` with a valid attr handling every known filesystem, network and scope bit, with all masks zero, with `size == 4`, with a 2-page struct, and with a bad `attr` pointer: each returns `ENOSYS`. After these calls the guest's next `open()` returns the same descriptor number as it would have before them, showing that no ruleset descriptor was allocated.
- T4 (covers F9): a best-effort sandboxing program (one that probes the ABI version and falls back when it gets `ENOSYS`) runs to completion under `hermit run --verify` with identical output in both runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/627: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_landlock_sandbox_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2322-2324 returns a fixed ENOSYS, the no-Landlock kernel answer, before any argument check and without consulting the host); linux_determinism stays conditional (host kernel build, boot LSM list and ABI version decide the native answer); 10 flag behaviours and 4 tests listed; the only guest, tests/rust/landlock.rs, has no ptrace runner in cargo tests or the e2e manifest at this SHA, so no coverage token and tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `landlock_create_ruleset`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+landlock_create_ruleset&body=Discussion+for+the+%60landlock_create_ruleset%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flandlock_create_ruleset.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60landlock_create_ruleset%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: landlock_create_ruleset" (https://github.com/rrnewton/hermit/issues/627), legacy variant B. The determinized ENOSYS arm carries `TODO-HUMAN-REVIEW(#827)` in the code (https://github.com/rrnewton/hermit/issues/827). Sibling bodies: landlock_add_rule (https://github.com/rrnewton/hermit/issues/626) and landlock_restrict_self (https://github.com/rrnewton/hermit/issues/628) share the same dispatch arm.

### Notes from the P3 reading (2026-10-03)

- `tests/e2e/manifests/inventory/test-files.json:3593-3595` says `tests/rust/landlock.rs` is "selected by a Rust integration test". No file under `hermit-cli/tests/` or `detcore/tests/` names `rustbin_landlock` or `landlock` at f4055eb9cae4, so that inventory entry is not true for this guest. Wiring it into an existing ptrace runner (for example the `default_cargo_*` list in `hermit-cli/tests/hermit_modes.rs`) would give T1 a citable path.
- The unit test `landlock_sandbox_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2344`) checks only the classification and the predicate, not the guest-visible result; it starts no Hermit run, so it cannot carry a coverage token.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
