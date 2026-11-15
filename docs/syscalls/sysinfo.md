---
schema: syscall-pages/v1
syscall: sysinfo
nr_x86_64: 99
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sysinfo&body=Discussion+for+the+%60sysinfo%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsysinfo.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sysinfo%60%2C+or+in+what+the+page+says%3F%0A
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

`sysinfo(info)` fills a `struct sysinfo` with system-wide statistics: uptime, load averages, memory and swap sizes, and the number of threads. Detcore builds the whole structure from its own model without a host call (`handle_sysinfo`, `detcore/src/syscalls/sysinfo.rs:400`): uptime from logical time, constant loads and thread count, and memory equal to the configured `--memory` size.

Manual reference: Linux man-pages sysinfo(2).

## Linux semantics

The call takes one pointer and no flags. The kernel fills the structure and copies it out; a bad pointer gives `EFAULT`. Each field reports host-wide state:

- `uptime`: seconds since boot (`CLOCK_BOOTTIME`, adjusted for the caller's time namespace), rounded up when there is a fractional part
- `loads[3]`: the 1, 5 and 15 minute load averages in fixed point, scaled by `1 << SI_LOAD_SHIFT` (65536)
- `totalram`, `freeram`, `sharedram`, `bufferram`, `totalswap`, `freeswap`, `totalhigh`, `freehigh`: in units of `mem_unit` bytes. On x86_64 the kernel scales them so that `mem_unit` is 1.
- `procs`: the number of threads on the whole system, truncated to 16 bits

### Flags and options

- F1: `uptime`: seconds since boot, rounded up, adjusted by the caller's time namespace
- F2: `loads`: 1, 5 and 15 minute load averages, fixed point with 16 fractional bits
- F3: memory fields and `mem_unit`: RAM, shared, buffer, swap and high-memory sizes, with `mem_unit` 1 on x86_64
- F4: `procs`: system-wide thread count, truncated to 16 bits
- F5: `info` NULL or not writable: `EFAULT`

## Determinism analysis

`linux_determinism: nondeterministic` (changed from the rendered `conditional`). Every field except `mem_unit` and the total sizes changes from one moment to the next on a live host: uptime advances, load averages and the thread count follow host activity, and free memory, buffers and swap follow allocation and page-cache behaviour by every process on the host. Even the total sizes differ between hosts.

### Nondeterminism sources

- Host boot time and the moment of the call (F1)
- Host load and scheduler activity (F2)
- Memory use by every host process, page cache and swap (F3)
- The number of threads on the host (F4)

## Hermit design

Target policy: `model` (changed from `investigate`). The guest sees a small, fixed machine whose uptime advances with logical time. The code at this SHA realizes it with no host call; `collect_sysinfo` (`detcore/src/syscalls/sysinfo.rs:439`) sets every field:

- F1: `uptime` is `--sysinfo-uptime-offset` (default 120, `detcore-model/src/config.rs:695`) plus the logical time elapsed since the epoch, rounded up to the next second as Linux does (`sysinfo_uptime_seconds`, `detcore/src/syscalls/sysinfo.rs:67`). It depends on elapsed logical time, not on the absolute epoch, so it repeats across separate `hermit run` invocations. If logical time were ever before the epoch, the helper returns a tool error rather than a value (`detcore/src/syscalls/sysinfo.rs:75`).
- F2: all three loads are the raw value 1, which is 1/65536 in Linux's fixed point, an almost idle machine.
- F3: total and free RAM both equal `--memory` (default "1GB", 1,000,000,000 bytes, `detcore-model/src/config.rs:701`), every other size is 0, and `mem_unit` is 1 (`configured_memory`, `detcore/src/syscalls/sysinfo.rs:486`). Its doc comment says the values match virtual `/proc/meminfo` and that Detcore does not model allocation pressure. The code carries `AUTONOMOUS-BOT-IMPLEMENTED` and `TODO-HUMAN-REVIEW(PR-2979)` tags (`detcore/src/syscalls/sysinfo.rs:466`).
- F4: `procs` is the constant 1.
- F5: a NULL pointer gives `EFAULT` (`detcore/src/syscalls/sysinfo.rs:405`); a non-NULL bad pointer gets `EFAULT` from the guest memory write.

Accepted differences from Linux, which are properties of the model rather than gaps: `freeram` never falls as the guest allocates, the configured memory is not enforced, and `procs` stays 1 however many threads the guest creates. A program that sizes work from `freeram` or `procs` sees the same small machine in every run.

`passthrough` would expose host uptime, load and memory, and `refuse` would break programs that query memory size (for example sort and JVM heap sizing), so neither is an alternative.

## Recommended tests

- T1 (covers F1): uptime is identical across `hermit run --verify` on ptrace, advances with logical time, and stays ordered across threads, a join and an `execve`.
  Existing candidates: `sysinfo_uptime_follows_thread_join_and_exec` at `hermit-cli/tests/hermit_modes.rs:662`, which runs `tests/c/sysinfo_uptime.c` twice with a pinned `--epoch`; cell `c-programs/sysinfo-uptime` (`tests/e2e/manifests/c-programs.yaml:9528`, verify ci on ptrace); unit test `sysinfo_uptime_rounds_positive_elapsed_up` at `detcore/src/syscalls/sysinfo.rs:514`.
- T2 (covers F2, F4): loads and `procs` are printed and compared across runs, and `procs` stays 1 after the guest creates threads and processes.
  Existing candidate: `check_sysinfo` in `tests/c/resource_determinism.c:430` (cell `c-programs/resource-determinism`, `tests/e2e/manifests/c-programs.yaml:7549`, verify ci on ptrace) prints every field, so verify compares them; it does not create threads first.
- T3 (covers F3): total and free RAM equal the configured memory before and after a large allocation, every other size is 0, and `mem_unit` is 1; with `--memory` set to another value, both `sysinfo` and `/proc/meminfo` follow it.
  Existing candidates: `tests/c/sysinfo.c:51` (cell `c-programs/sysinfo`, `tests/e2e/manifests/c-programs.yaml:9470`, verify ci on ptrace); `check_sysinfo_memory_matches_configured_memory` in `tests/c/resource_determinism.c:458`; unit test `sysinfo_memory_matches_configured_memory` at `detcore/src/syscalls/sysinfo.rs:756`. None of them changes `--memory`.
- T4 (covers F5): `sysinfo(NULL)` and an unmapped pointer return `EFAULT`.
  Existing candidate: `tests/c/sysinfo.c:73` covers NULL only.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (uptime from logical time plus a fixed offset, constant loads and procs, memory equal to --memory, no host call); linux_determinism conditional -> nondeterministic (live host-wide statistics); 5 field behaviours and 4 tests listed; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sysinfo`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sysinfo&body=Discussion+for+the+%60sysinfo%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsysinfo.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sysinfo%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sysinfo`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
