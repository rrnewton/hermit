# DynamoRIO Backend Test Status

Status date: 2026-07-22.

This page tracks Hermit's test suite under the experimental Reverie DynamoRIO
backend. DBI executes and instruments Linux programs, but most Detcore syscall
virtualization and deterministic scheduling policies are not connected yet.
A passing functional test therefore establishes DBI compatibility, not full
determinism parity with ptrace.

## Reproduction

Build the native client, then select DBI for Hermit subprocesses launched by
Cargo integration tests:

```bash
export DYNAMORIO_HOME=/path/to/dynamorio
export REVERIE_DBI_CLIENT=/path/to/libreverie_dbi_client.so
export HERMIT_BACKEND=dbi
cargo test -p hermit --test TEST_TARGET -- --test-threads=1
```

Run targets separately with an outer timeout. `clock_determinism` currently
blocks in `clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, ...)`, so one monolithic
`cargo test -p hermit` process cannot reach later targets. `HERMIT_BACKEND`
only affects `run`; record/replay tests continue to exercise their ptrace
implementation.

Set `REVERIE_DBI_SUMMARY=1` when branch/syscall totals are needed. Summaries are
off by default because the branch count is diagnostic data and varies between
runs; emitting it on guest stderr caused false `--verify` failures.

## Progression

The baseline is the first complete per-target survey after basic `/bin/true`,
`/bin/echo`, and `/bin/ls` support. The final column includes the focused fixes
through this follow-up.

| Cargo target | Baseline | Current | Notes |
| --- | ---: | ---: | --- |
| Hermit library tests | 10 pass | 10 pass | No guest DBI execution |
| Hermit binary unit tests | 23 pass | 23 pass | No guest DBI execution |
| `arbitrary_binaries` | 2 pass | 2 pass | Run matrix uses DBI; record/replay case uses ptrace |
| `cli` | 15 pass, 1 fail | 16 pass | Ptrace capability test now selects ptrace explicitly |
| `clock_determinism` | 1 timeout | 1 timeout | Process CPU-clock sleep limitation |
| `epoll_determinism` | 5 pass | 5 pass | Full target passes |
| `hermit_modes` | 52 pass, 11 fail, 8 ignored | 58 pass, 5 fail, 8 ignored | Six DBI guest cases fixed |
| `ipc_determinism` | 1 fail | 1 fail | Pipe writer order changes between runs |
| `mmap_determinism` | 2 pass, 3 fail | 5 pass | Guest ASLR disabled |
| `procfs_determinism` | 2 pass, 4 fail | 6 pass | Stable snapshots for four volatile files |
| `random_determinism` | 1 fail | 1 pass | Seeded `getrandom` and random-device reads |
| `record_replay` | 17 pass | 17 pass | Ptrace record/replay, not DBI coverage |
| `signal_determinism` | 3 pass, 2 fail | 3 pass, 2 fail | Pending/exec and interval-timer gaps |
| `stress_suite` | 3 ignored | 3 ignored | Explicit stress tiers |
| `thread_sync_determinism` | 1 fail | 1 fail | Barrier completion order changes |
| **Full Cargo inventory** | **131 pass, 24 fail, 1 timeout, 11 ignored** | **146 pass, 9 fail, 1 timeout, 11 ignored** | Includes non-DBI tests |
| **DBI guest-execution subset** | **65 pass, 23 fail, 1 timeout, 8 ignored** | **79 pass, 9 fail, 1 timeout, 8 ignored** | Excludes unit/CLI-only and ptrace record/replay cases |

The initial survey recovered five DBI guest tests:

- `default_shell_parallel_work`
- `default_shell_taskset`
- `verify_honors_tmp_and_environment`
- `default_virtualized_uname`
- `default_network_bind`

This follow-up recovers nine more:

- `map_shared_address_is_deterministic`
- `mmap_reuses_unmapped_address_deterministically`
- `multiple_mmap_addresses_are_deterministic`
- `proc_cpuinfo_is_deterministic`
- `proc_self_maps_is_deterministic`
- `proc_self_stat_is_deterministic`
- `proc_self_status_is_deterministic`
- `random_sources_repeat_across_runs_and_change_with_seed`
- `verify_mode_matrix`

## Fixes Applied

- Added `HERMIT_BACKEND` as the environment form of the global backend option.
- Launch shebang programs through their interpreter because DynamoRIO requires
  an ELF application entrypoint.
- Preserve Hermit's exact cleared/minimal guest environment when wrapping the
  application with `drrun`.
- Made instrumentation summaries opt-in so tooling diagnostics do not become
  nondeterministic guest stderr.
- Rewrote `uname.release` to Detcore's `5.2.0` value.
- Assigned zero-port IPv4/IPv6 binds from the deterministic ephemeral range and
  advanced that range past explicit bindings.
- Disabled ASLR before launching `drrun`, stabilizing non-fixed mappings.
- Replaced four volatile procfs views with minimal stable memfd snapshots.
- Passed Hermit's RNG seed to DBI and virtualized `getrandom`, `/dev/random`,
  and `/dev/urandom` reads with deterministic per-buffer streams.
- Zeroed successful `getrusage` output and returned fixed `sysinfo` metadata.

## Remaining Failure Categories

| Category | Tests | Required work |
| --- | --- | --- |
| Deterministic scheduling | `ipc_patterns_are_deterministic_across_five_runs`, `thread_sync_patterns_are_deterministic_across_five_runs`, `hello_race_chaos_verify` | Connect Detcore scheduling and thread lifecycle to DBI |
| Time | `clock_apis_are_deterministic_across_five_runs` | Virtualize clock syscalls; process CPU-clock sleep currently blocks |
| Networking diagnostics | `default_lit_networking` | Connect Detcore networking diagnostics |
| Signals/timers | `pending_signal_and_mask_survive_exec`, `sigalrm_itimer_delivery_is_deterministic`, `no_hardware_stacktrace_signal` | Signal, exec lifecycle, interval timers, and stack capture |
| Resource limits | `resource_syscalls_are_deterministic_across_five_runs` | Reconcile DynamoRIO client and application rlimit views before virtualizing `prlimit64` |
| Backtraces/events | `no_hardware_minimal_hello_backtraces` | DBI event recording and stack support |

Passing targets should remain in the DBI survey even when their assertions do
not require determinism. They catch instruction-rewriting, syscall forwarding,
threading, signal, and dynamic-runtime regressions before deeper policy parity
is available.
