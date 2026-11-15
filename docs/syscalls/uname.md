---
schema: syscall-pages/v1
syscall: uname
nr_x86_64: 63
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+uname&body=Discussion+for+the+%60uname%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funame.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60uname%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`uname(2)` fills a `struct utsname` with the kernel name, host name, kernel release, kernel version, machine and NIS domain name. Hermit lets the kernel fill the buffer, then overwrites the release and version with fixed values, and the host and domain names too when the guest has no UTS namespace of its own. The result does not depend on the host kernel or host name.

## Linux semantics

The kernel copies six fixed-size (65-byte) strings from the caller's UTS namespace into `buf`: `sysname` ("Linux"), `nodename`, `release`, `version`, `machine` and `domainname`. The call has no side effects and returns 0, or `-EFAULT` when `buf` cannot be written. `nodename` and `domainname` are per-UTS-namespace state that `sethostname(2)`/`setdomainname(2)` change. `release` and `version` describe the running kernel build. Two personality bits change the output: `UNAME26` reports a 2.6.x release for a 3.x or later kernel, and `PER_LINUX32` reports a 32-bit `machine` ("i686") on x86_64.

### Flags and options

- F1: a writable `buf` receives all six fields and the call returns 0.
- F2: a NULL or unwritable `buf` fails with `EFAULT` and writes nothing.
- F3: `nodename` and `domainname` come from the caller's UTS namespace, as last set by `sethostname`/`setdomainname`.
- F4: `release` and `version` describe the running kernel; under the `UNAME26` personality `release` is rewritten to a 2.6.x string.
- F5: `sysname` is always "Linux"; `machine` is the hardware name ("x86_64"), or "i686" under the `PER_LINUX32` personality.

## Determinism analysis

On one host, with no `sethostname`/`setdomainname` in between, repeated calls return identical bytes. Across hosts the output changes: `nodename` is the machine's host name, `release` and `version` change with every kernel build and boot image, and `domainname` is host configuration. A program that prints, hashes or branches on these fields (build systems, `config.guess`, version banners) is therefore reproducible only on the same host. `machine` is stable across x86_64 hosts. `linux_determinism` is `conditional`: deterministic for a fixed host and UTS namespace, different across hosts.

## Hermit design

`target_policy` is `emulate`: Hermit serves fabricated values for the host-dependent fields rather than letting the host's leak through. Code at hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2529`, which calls `handle_uname` at `detcore/src/syscalls/misc.rs:522-548`.
- `detcore/src/syscalls/misc.rs:528` runs the real call through `record_or_replay`, so a kernel `EFAULT` (F2) returns before Hermit touches memory. When `buf` is non-NULL, `misc.rs:529-544` reads the filled struct back and rewrites it.
- `misc.rs:535-540`: when `has_uts_namespace` is false, `nodename` becomes `DEFAULT_HOSTNAME` ("hermetic-container.local", `detcore/src/consts.rs:27`) and `domainname` becomes its last dot component, "local".
- When Hermit owns a UTS namespace, the namespace was created with the same names by `default_container` (`hermit-cli/src/bin/hermit/container.rs:442-448`). `has_uts_namespace` is set at `hermit-cli/src/bin/hermit/run.rs:3644-3656`: it is true unless `--no-namespace` is given or the backend cannot apply the UTS host name. The comment there says guest `sethostname`/`setdomainname` are refused with `EPERM`, so F3 is fixed for the guest's lifetime.
- `misc.rs:542` always sets `release` to "5.2.0". `misc.rs:543` sets `version` to `#1 SMP <epoch>`, formatted from the configured epoch in UTC (`misc.rs:531-533`).
- `sysname` and `machine` pass through from the kernel (F5). They are the same on every x86_64 host, so no rewrite is needed.

Target (owner decision 11, 2026-10-04): `sethostname` and `setdomainname` are emulated with a per-run name, and `uname` reports it (F3). Today both setters are refused with a fixed `EPERM` (`is_privileged_admin_refused_syscall`, `detcore/src/syscall_classification.rs:902`, entries at `detcore/src/syscall_classification.rs:916`-`917`), so a guest's `hostname foo` fails and `nodename`/`domainname` never change. At the target Detcore keeps one `nodename` and one `domainname` for the run, starting at `DEFAULT_HOSTNAME` ("hermetic-container.local", `detcore/src/consts.rs:27`) and "local", the values the guest reads today. A `sethostname`/`setdomainname` of at most 64 bytes (`__NEW_UTS_LEN`) replaces the stored name, and a longer one gets `EINVAL`, as on Linux for the uid-0 owner of a UTS namespace. `handle_uname` then writes both fields from that record in every mode, instead of passing the kernel's through when `has_uts_namespace` is true and rewriting them only when it is false (`detcore/src/syscalls/misc.rs:535`-`540`). The host's and the container namespace's real names are never changed, so the value cannot leak across runs. One record per run is enough while a guest cannot create a UTS namespace of its own (`CLONE_NEWUTS` gets `EPERM`, owner decision 15).

Known divergence: because `release` is overwritten unconditionally, a guest that sets `UNAME26` still sees "5.2.0", not a 2.6.x string (F4). A guest that sets `PER_LINUX32` sees the kernel's "i686", since `machine` is not rewritten.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on DBT the guest shares the host's UTS namespace (`has_uts_namespace` false, `hermit-cli/src/bin/hermit/run.rs:3656`), but Detcore rewrites `nodename` and `domainname` to the fixed names (`detcore/src/syscalls/misc.rs:535-540`), so `uname` output matches the strict container there and that mode needs no test of its own.

## Recommended tests

- T1 (covers F1, F3, F5): under `hermit run --verify` on ptrace, call `uname` and check `sysname` "Linux", `nodename` "hermetic-container.local", `machine` "x86_64", and that two runs print identical bytes.
  Existing candidate: `tests/c/uname.c` (cell `c-programs/uname`, `tests/e2e/manifests/c-programs.yaml:10367`, verify ci on ptrace), which checks `sysname`, `nodename`, `release` and `machine`. It does not check `domainname`.
- T2 (covers F3): check `domainname` "local" and that `gethostname`/`getdomainname` agree with `uname`, under `hermit run --strict` on ptrace in its container, where Hermit owns the UTS namespace.
  Existing candidate: `tests/c/uname_identity.c` (cell `c-programs/uname-identity`, `tests/e2e/manifests/c-programs.yaml:14224`, verify ci on ptrace). `hermit-cli/tests/hermit_modes.rs:983` (`default_virtualized_uname`) and `detcore/tests/lit/uname.test` check `uname -nr` without and with `--no-sequentialize-threads`.
- T3 (covers F4): check `release` "5.2.0", that `version` is `#1 SMP` followed by the configured epoch in UTC, and that a different configured epoch changes only the date part of `version`. Then set `personality(UNAME26)` and record what `release` reads; Linux would give a 2.6.x string.
- T4 (covers F2): a raw `syscall(SYS_uname, NULL)` and a call with an unmapped pointer both return `EFAULT`, and Hermit does not crash or rewrite memory.
- T5 (covers F5): set `personality(PER_LINUX32)` and check that `machine` reads "i686", as on Linux, then restore the personality.
- T6 (covers F3): under `hermit run --verify` on ptrace, `sethostname("guest-a", 7)` and `setdomainname("example", 7)` return 0, after which `uname` reports `nodename` "guest-a" and `domainname` "example" in the caller and in a child forked afterwards, `gethostname`/`getdomainname` agree, and both runs print identical bytes (owner decision 11). Fails at this SHA: both setters return `EPERM` (`detcore/src/syscall_classification.rs:916`-`917`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy emulate (kernel fills utsname, Detcore overwrites release 5.2.0, version from the epoch, and nodename/domainname without a UTS namespace); linux_determinism stays conditional (host-dependent fields); 5 flag behaviours and 5 tests listed; UNAME26 is masked by the fixed release; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (host UTS namespace under --no-namespace and DBT, output unchanged through the nodename/domainname rewrite); target_policy unchanged (emulate); T2 narrowed to the strict ptrace container (dropped its --no-namespace half); no new F/T ids; no open question removed
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 11 applied (uname reports the per-run nodename/domainname that emulated sethostname/setdomainname set, replacing today's EPERM refusal and the has_uts_namespace passthrough); target_policy stays emulate; new T6; no open question removed (UNAME26 question kept)
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/uname.c, T1 covered by tests/c/host_identity.c, T1 covered by tests/c/uname_identity.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `uname`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+uname&body=Discussion+for+the+%60uname%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funame.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60uname%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `uname`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Notes from the p3 review (2026-10-03, hermit f4055eb9cae4)

- `tests/c/uname_identity.c` says in its header, and the cell's DBT disabled reason at `tests/e2e/manifests/c-programs.yaml:14248` says, that DBT leaks the host node name. At this SHA, `hermit-cli/src/bin/hermit/run.rs:3644-3656` turns `has_uts_namespace` off for DBT, which makes `detcore/src/syscalls/misc.rs:535-540` rewrite the node name. That reason may be stale; this is a reading of the code, not a run.
- `record_or_replay` replays the recorded struct and then applies the same rewrite, so record and replay see the same bytes. `hermit-cli/tests/record_replay.rs:317` lists `c_uname` in `BASELINE_RECORD_WORKLOADS`.

### Open questions

- Should Hermit honour `UNAME26` by reporting a 2.6.x release when the guest sets it, or is a fixed "5.2.0" the intended contract? Today the fixed value wins. This is minor and does not block the policy.
