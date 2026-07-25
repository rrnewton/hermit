<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# CI and validate.sh alignment

The Rust workflow and local validation use the same capability selectors:

| Lane | Runner | Command | Capability contract |
| --- | --- | --- | --- |
| Portable | GitHub-hosted Ubuntu | `./validate.sh --hosted-only --no-label-pr` | No PMU counters, ptrace CPUID faulting, or KVM required |
| Hardware | self-hosted `[Linux, X64, hermit, pmu]` | `./validate.sh --hardware-only --no-label-pr` | PMU, CPUID, KVM, or another host capability is part of the test |

The default `./validate.sh` remains the developer superset. The focused modes
exist so either CI subset can be reproduced independently. The tiered workflow
maps `quick` to portable, `full` to per-PR hardware, and long database and
PMU chaos stress to the weekly self-hosted `super` profile.

## Portable lane

The hosted job enables user and mount namespaces before starting Hermit. This
is a privilege prerequisite, not a PMU or CPUID dependency. Guest tests in this
lane either need no hardware event handling or explicitly pass:

```text
--max-timeslice=disabled --no-virtualize-cpuid
```

The selector covers exactly 478 of the 803 Cargo-discovered cases:

| Group | Cases | Selector |
| --- | ---: | --- |
| Workspace unit, bin, and doc baseline | 280 | Existing regular-job selection |
| Detcore misc without CPUID probes | 21 | `tests_misc`, excluding two RDRAND/CPUID cases |
| Detcore parallel without RCB scheduling | 5 | Raw/noop cases, excluding generated `detcore` variants |
| Flaky guest crate contract | 1 | The crate's standalone Cargo test |
| Portable Hermit integration cases | 171 | Non-KVM CLI, 67 non-PMU modes, apps, commands, IPC, time, memory, procfs, signals, Python, stress, and rr source contract |

The same lane enforces the 12 portable L1-L4 working-envelope cells and runs
the 147-row strict compatibility corpus with PMU and CPUID explicitly disabled.
The compatibility corpus retains its existing informational policy.

The lane also requires all six DynamoRIO DBI parity scenarios currently
marked `pass`. Cargo builds the pinned DynamoRIO runtime and native client;
external `DYNAMORIO_HOME`, `HERMIT_DRRUN`, and `HERMIT_DBI_CLIENT` variables are
not part of the CI contract.

## Hardware lane

The remaining 325 Cargo cases require hardware. The per-PR selector executes
311, the weekly `super` tier executes 11 long cases, and three rr cases remain
documented gaps:

| Group | Cases | Routing | Hardware reason |
| --- | ---: | --- | --- |
| Detcore CPUID/RDRAND probes | 2 | Per-PR | Host feature probe and deterministic masking |
| Detcore time tests | 14 | Per-PR | Nonzero RCB preemption configuration |
| Detcore parallel variants | 11 | Per-PR | Deterministic RCB preemption assertions |
| KVM CLI cases | 16 | Per-PR | Read/write `/dev/kvm` is required |
| Buck chaos variants | 8 | Weekly | Explicit one-million-RCB time slice |
| Runtime, database, scheduling, and syscall targets | 46 | Per-PR | Default PMU/CPUID or record/replay configuration |
| Ignored runtime/database/analyze tiers | 14 | 11 per-PR, 3 weekly | Default PMU/CPUID configuration |
| Slow CAS stress | 1 | Per-PR | PMU preemption search and replay |
| rr syscall corpus | 213 | 210 per-PR, 3 known gaps | Explicit 80-million-RCB time slice |

The Detcore time and parallel commands intentionally do not pass `--ignored`.
Those targets contain no ignored tests; the former workflow selected zero
cases. The time target is serialized because its in-process tracee forks
deadlock under the Rust harness's parallel test threads. The memory families
are serialized because concurrent PMU guests caused counter contention on the
self-hosted machine, and hardware mode uses a one-hour per-gate timeout for
those CPU-heavy fixtures.

The per-PR hardware lane runs LevelDB's bounded `env_posix_test`. The eight
Buck chaos cases, PMU-skid-sensitive `analyze_hello_race`, full randomized
LevelDB suite, and SQLite veryquick suite remain in the weekly `super` profile
because they cannot fit the per-PR job budget on the self-hosted runner.
The rr gate excludes `rr_ppoll` (unsupported `ppoll` operation), `rr_rlimit`
(host policy rejects `setrlimit`), and `rr_sched_yield_to_lower_priority` (priority scheduling gap).

The hardware lane also gates the three record/replay working-envelope cells,
the 128-row R/R compatibility corpus, debugger
integration, and ptrace backend parity. Missing rr sources, namespaces, PMU,
CPUID, KVM, or runtime prerequisites fail the lane instead of silently reducing
coverage.

## Workflow prerequisites

The hosted job installs the Unix commands, language tools, app binaries,
cargo-nextest, rustfmt, and Clippy used by its selected tests. The self-hosted
job initializes the rr submodule and preflights PMU, KVM, namespaces, runtime
tools, databases, and debuggers before starting the hardware selector.

When adding a test:

1. Identify whether it asserts PMU/RCB, CPUID, KVM, or another host capability.
2. Put it in exactly one validation tier.
3. If it is portable, make the disabling flags explicit in its command helper.
4. Run both focused modes on a capable host and the default full validation.
5. Update the case totals in this document.
