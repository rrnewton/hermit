---
schema: syscall-pages/v1
syscall: personality
nr_x86_64: 135
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/317
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+personality&body=Discussion+for+the+%60personality%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpersonality.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60personality%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`personality(persona)` reads or replaces the calling thread's persona: an execution domain in the low byte plus flag bits that change how later `execve`, `mmap`, `uname` and `select` behave. Detcore passes it to the kernel unchanged (`detcore/src/lib.rs:2843`). The persona Hermit starts the guest with is not the host default: the ptrace tracer sets `PER_LINUX | ADDR_NO_RANDOMIZE` before the first exec, and that flag is what keeps the guest's address layout fixed. A guest can clear it, and nothing in Hermit puts it back.

## Linux semantics

The persona is a 32-bit value stored per task. The argument is truncated to 32 bits. `0xffffffff` only reads it; any other value replaces the whole persona and the call returns the previous one. On x86_64 the kernel does not validate the value: any bits are stored and read back, and the call does not fail in practice. The persona is copied to children by `fork` and `clone` and survives `execve`, except that an exec that gains privilege (setuid, setgid or file capabilities) clears `ADDR_NO_RANDOMIZE`, `ADDR_COMPAT_LAYOUT`, `READ_IMPLIES_EXEC` and `MMAP_PAGE_ZERO`. Under `PR_SET_NO_NEW_PRIVS` an exec never gains privilege, so those flags are not cleared.

Most flags take effect only at the next `execve`. At exec the kernel randomizes the stack, `mmap` base, vDSO and heap start unless `ADDR_NO_RANDOMIZE` is set or `kernel.randomize_va_space` is 0.

Host results measured for this issue on Linux 7.1.3: the query returns `0x0` for a normal shell child; `0x1ffffffff` acts as a query; setting `0x123456` returns the old value and a later query returns `0x123456`.

### Flags and options

- F1: `persona == 0xffffffff`: query only; returns the current persona and changes nothing
- F2: the argument is truncated to 32 bits, so a 64-bit value whose low 32 bits are `0xffffffff` (for example `0x1ffffffff`) is also a query
- F3: any other value replaces the whole persona and returns the previous one; there is no validation, unknown bits are stored and read back, and the call does not fail
- F4: execution domain (`persona & PER_MASK`): `PER_LINUX` (0) is the default; `PER_LINUX32` makes `uname` report `i686` in `machine`; other domain numbers are stored but change nothing else on x86_64
- F5: `ADDR_NO_RANDOMIZE`: the next `execve` lays out stack, `mmap` base, vDSO and heap without randomization; clearing it makes the next `execve` randomize them when `kernel.randomize_va_space` is nonzero
- F6: `ADDR_COMPAT_LAYOUT`: the next `execve` uses the legacy bottom-up `mmap` layout
- F7: `READ_IMPLIES_EXEC`: `PROT_READ` mappings also get `PROT_EXEC`; on x86_64 `execve` sets or clears it from the new program's `PT_GNU_STACK`
- F8: `MMAP_PAGE_ZERO`: the next `execve` maps page 0 read-only
- F9: `UNAME26`: `uname` reports a `2.6.x` release string instead of the real one
- F10: `STICKY_TIMEOUTS`: `select` and `pselect6` do not write the remaining time back to the timeout argument
- F11: `ADDR_LIMIT_32BIT`, `ADDR_LIMIT_3GB`, `SHORT_INODE`, `WHOLE_SECONDS`, `FDPIC_FUNCPTRS`: stored, inherited and read back; this issue found no other effect on 64-bit x86_64 processes
- F12: inheritance: copied by `fork` and `clone`, kept by `execve`; a privilege-gaining exec clears the four flags named above, which `PR_SET_NO_NEW_PRIVS` prevents

## Determinism analysis

`linux_determinism: conditional`. The call itself is a function of its argument and of the persona the task inherited, so it repeats when the starting persona is fixed. Natively the starting persona comes from the parent (a launch under `setarch -R` starts with `ADDR_NO_RANDOMIZE`). The larger effect is F5: the persona decides whether every later `execve` gets a random address layout. A program that clears `ADDR_NO_RANDOMIZE` and then execs gets a different stack, `mmap` base, vDSO and heap address on every run.

### Nondeterminism sources

- The persona inherited from the launching process (F1, F3).
- ASLR at the next `execve` when `ADDR_NO_RANDOMIZE` is clear and `kernel.randomize_va_space` is nonzero (F5).
- `kernel.randomize_va_space`, a host setting (F5).

## Hermit design

Target policy: `model` (the code at hermit f4055eb9cae4 passes it through).

What the code does today:

- `classify_syscall` returns `PassThrough` for `personality` in the "ISSUE-REVIEWED PASS-THROUGH SYSCALLS" block (`detcore/src/syscall_classification.rs:820`). The comment at `detcore/src/syscall_classification.rs:808` says these remain conditional on Hermit's fixed-container assumptions, and `detcore/src/syscall_classification.rs:811` still carries `TODO-HUMAN-REVIEW(PR-643)`. The unit test `representative_policies_stay_in_their_reviewed_sections` pins the classification (`detcore/src/syscall_classification.rs:2109`).
- Dispatch is the generic arm at `detcore/src/lib.rs:2843`, which calls `passthrough` (`detcore/src/lib.rs:367`). Under record and replay, `passthru_opt` (`detcore/src/lib.rs:3140`) leaves it unsubscribed (`detcore/src/lib.rs:1084`).
- The starting persona comes from Reverie, not Detcore. In Reverie at the rev pinned by `Cargo.lock:1377` (d766df20b780), `init_tracee` in `reverie-ptrace/src/tracer.rs` sets `PR_SET_NO_NEW_PRIVS` (line 3971) and then calls `personality(PER_LINUX | ADDR_NO_RANDOMIZE)` (line 3976). It runs once, before the guest's first exec, and replaces the whole persona. No Rust file in the export names `personality` or `ADDR_NO_RANDOMIZE` except the classification table and `hermit-cli/tests/rr_suite.rs`.
- So a guest query returns `0x40000` under Hermit where a normal native launch returns `0x0`. That value is fixed, and `tests/c/personality_domain.c:12` already relies on it ("Hermit already enables ADDR_NO_RANDOMIZE").

Why `model` and not `passthrough`:

- **ASLR escape (F5).** A guest that calls `personality(PER_LINUX)` and then `execve` gives the new program a randomized layout, because nothing re-applies `ADDR_NO_RANDOMIZE`. Every address that program prints or hashes then differs between runs. This follows from the code above; it was not run under Hermit for this issue.
- **`UNAME26` is lost (F9).** `handle_uname` sets `release` to `"5.2.0"` unconditionally (`detcore/src/syscalls/misc.rs:541`), so the flag that asks for a `2.6.x` string has no effect under Hermit. This is deterministic but differs from Linux. `machine` is not rewritten, so `PER_LINUX32` (F4) still works.
- A model fixes both: Detcore keeps a virtual persona per task (inherited at `clone`, kept at `execve`), returns it for F1 to F3, always keeps `ADDR_NO_RANDOMIZE` set in the real persona, and lets `handle_uname` read the virtual `UNAME26` bit.

### Gaps and open points

- `STICKY_TIMEOUTS` (F10): the `select` and `pselect6` arms (`detcore/src/lib.rs:2615`, `detcore/src/lib.rs:2612`) go to Detcore handlers, and no Rust file in the export names the flag. Whether those handlers write the remaining time back, and so ignore the flag, was not checked.
- `rr_personality` (`hermit-cli/tests/rr_suite.rs:376`) is marked `#[ignore]` by its `rr_test!` macro (`hermit-cli/tests/rr_suite.rs:230`), so it is not coverage.
- The mechanism and the starting value the guest should see are owner questions; see Open questions under Additional information.

## Recommended tests

- T1 (covers F1, F2, F3): query with `0xffffffff` and with `0x1ffffffff`; set `0x123456`, check that the set returns the old persona and the query returns `0x123456`; restore. Identical across runs under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/personality_domain.c` (cell `c-programs/personality-domain`, `tests/e2e/manifests/c-programs.yaml:11063`, verify ci on ptrace) does the query and a start, target, start round trip with `UNAME26`, through glibc `personality`, and prints the starting persona.
- T2 (covers F4, F9): with `PER_LINUX32`, `uname` reports `machine == "i686"`; with `UNAME26`, `uname` reports a `2.6.x` release as Linux does. The second check fails at this SHA because of `detcore/src/syscalls/misc.rs:541`.
- T3 (covers F5, F12): parent clears `ADDR_NO_RANDOMIZE`, forks, and the child execs a helper that prints its persona, the address of a stack variable, of a fresh anonymous `mmap`, of `sbrk(0)` and of the vDSO. Under `--verify` both runs must print the same addresses. Today this is expected to fail.
- T4 (covers F6, F8): set `ADDR_COMPAT_LAYOUT`, then `MMAP_PAGE_ZERO`, exec a helper that reports its first anonymous `mmap` address and whether `/proc/self/maps` has a mapping at address 0. Identical across runs and matching Linux.
- T5 (covers F7): set `READ_IMPLIES_EXEC`, `mmap` a `PROT_READ` page and check `/proc/self/maps` shows it executable; exec a program with a non-executable stack and check the flag is cleared.
- T6 (covers F10): with `STICKY_TIMEOUTS`, `select` with no descriptors and a 10 ms timeout leaves the `timeval` unchanged; without it the `timeval` is written back. Results match Linux and repeat.
- T7 (covers F11): set each of `ADDR_LIMIT_32BIT`, `ADDR_LIMIT_3GB`, `SHORT_INODE`, `WHOLE_SECONDS` and `FDPIC_FUNCPTRS`, read it back, fork and exec, and read it back again.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/317#issuecomment-5872899491
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/317: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; personality records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/317 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic -> conditional (clearing ADDR_NO_RANDOMIZE makes the next execve's layout random); target_policy passthrough -> model (Reverie sets PER_LINUX|ADDR_NO_RANDOMIZE once before the first exec and nothing re-applies it; UNAME26 is lost to handle_uname's fixed release); 12 flag behaviours and 7 tests; mechanism and starting persona are open questions for the owner, tracking_status todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `personality`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+personality&body=Discussion+for+the+%60personality%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpersonality.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60personality%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: personality" (https://github.com/rrnewton/hermit/issues/317), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/317.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=personality`, and by confirming hermit currently routes `personality` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
personality(0xffffffff)         = 0 (PER_LINUX)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/317#issuecomment-5872899491

### Open questions

For the owner (asked 2026-10-03 by syscall-tracking-p3; tracking_status stays todo until answered):

1. Mechanism. Should Detcore (a) keep a virtual persona per task and always OR `ADDR_NO_RANDOMIZE` into the real one, (b) keep passing the call through but re-apply `ADDR_NO_RANDOMIZE` by injection before forwarding each `execve`, or (c) refuse a persona that clears `ADDR_NO_RANDOMIZE`? Only (a) also lets `uname` honour `UNAME26`.
2. Starting value. Should a guest's first query return `0x40000` (today's value, which Reverie sets and `tests/c/personality_domain.c` prints) or `0x0` (what a normal native launch returns)?
