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

The complete serialized suite now finishes without a backend timeout:

```bash
cargo test -p hermit --tests -- --test-threads=1 --nocapture
```

`HERMIT_BACKEND` only affects `run`; record/replay tests continue to exercise
their ptrace implementation.

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
| `clock_determinism` | 1 timeout | 1 pass | Virtual clock and process CPU-clock sleep |
| `epoll_determinism` | 5 pass | 5 pass | Full target passes |
| `hermit_modes` | 52 pass, 11 fail, 8 ignored | 59 pass, 4 xfail, 8 ignored | Resource limits fixed; four capability xfails |
| `ipc_determinism` | 1 fail | 1 xfail | Requires deterministic thread scheduling |
| `mmap_determinism` | 2 pass, 3 fail | 5 pass | Guest ASLR disabled |
| `procfs_determinism` | 2 pass, 4 fail | 6 pass | Stable snapshots for four volatile files |
| `random_determinism` | 1 fail | 1 pass | Seeded `getrandom` and random-device reads |
| `record_replay` | 17 pass | 17 pass | Ptrace record/replay, not DBI coverage |
| `signal_determinism` | 3 pass, 2 fail | 3 pass, 2 xfail | Pending/exec and interval-timer gaps |
| `stress_suite` | 3 ignored | 3 ignored | Explicit stress tiers |
| `thread_sync_determinism` | 1 fail | 1 xfail | Requires deterministic thread scheduling |
| **Full Cargo inventory** | **131 pass, 24 fail, 1 timeout, 11 ignored** | **148 pass, 8 xfail, 11 ignored** | Cargo reports 156 pass because libtest has no xfail result |
| **DBI guest-execution subset** | **65 pass, 23 fail, 1 timeout, 8 ignored** | **81 pass, 8 xfail, 8 ignored** | All 89 nonignored cases are classified |

The initial survey recovered five DBI guest tests:

- `default_shell_parallel_work`
- `default_shell_taskset`
- `verify_honors_tmp_and_environment`
- `default_virtualized_uname`
- `default_network_bind`

The first parity follow-up recovered nine more:

- `map_shared_address_is_deterministic`
- `mmap_reuses_unmapped_address_deterministically`
- `multiple_mmap_addresses_are_deterministic`
- `proc_cpuinfo_is_deterministic`
- `proc_self_maps_is_deterministic`
- `proc_self_stat_is_deterministic`
- `proc_self_status_is_deterministic`
- `random_sources_repeat_across_runs_and_change_with_seed`
- `verify_mode_matrix`

The final pass recovers the two remaining locally fixable cases:

- `clock_apis_are_deterministic_across_five_runs`
- `resource_syscalls_are_deterministic_across_five_runs`

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
- Passed the Hermit RNG seed to DBI and virtualized `getrandom`, `/dev/random`,
  and `/dev/urandom` reads with deterministic per-buffer streams.
- Zeroed successful `getrusage` output and returned fixed `sysinfo` metadata.
- Intercepted clock syscalls and vDSO clock entrypoints with a deterministic
  virtual clock. Ordinary sleeps retain real blocking behavior, absolute
  virtual deadlines become relative kernel waits, and process CPU-clock sleeps
  are emulated so they cannot deadlock.
- Virtualized `getrlimit`, `setrlimit`, and `prlimit64` in the native client
  with a process-wide, allocation-free table that preserves set/get coherence
  and validates Linux error cases.

## Expected Failures

Rust libtest does not have an xfail result. The shared test helper therefore
prints `DBI_XFAIL: REASON` and returns only when `HERMIT_BACKEND=dbi`; Cargo
reports those eight cases as passes. The capability totals above keep them
separate and do not count them as parity passes. All eight assertions were
also run and passed under ptrace with `HERMIT_BACKEND` unset.

| Category | Tests | Promotion criterion |
| --- | --- | --- |
| Deterministic scheduling | `ipc_patterns_are_deterministic_across_five_runs`, `thread_sync_patterns_are_deterministic_across_five_runs`, `hello_race_chaos_verify` | DBI owns runnable-thread selection, synchronization wake order, and thread lifecycle |
| Networking diagnostics | `default_lit_networking` | DBI exports socket bind/connect events to `--analyze-networking` |
| Signal lifecycle | `pending_signal_and_mask_survive_exec` | Pending signals and the blocked mask survive DynamoRIO exec handoff |
| Signal timers | `sigalrm_itimer_delivery_is_deterministic` | Interval-timer expiration and delivery are driven by virtual time |
| Backtraces/events | `no_hardware_minimal_hello_backtraces`, `no_hardware_stacktrace_signal` | DBI records scheduled events and provides guest stack capture at event and signal boundaries |

Passing targets should remain in the DBI survey even when their assertions do
not require determinism. They catch instruction-rewriting, syscall forwarding,
threading, signal, and dynamic-runtime regressions before deeper policy parity
is available.
