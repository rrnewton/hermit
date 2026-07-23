# Record/Replay + Chaos Coverage Matrix (honest)

Task: `impl-expand-rr-chaos-coverage` — expand record/replay and chaos test
coverage on the guest workloads that *already* pass, rather than chasing large
apps.

- Worktree: `worktrees/slot87`, branch `impl-expand-rr-chaos-coverage`, based on
  frontier `344200e`.
- Host: `perf_event_paranoid=1` (PMU counters available). Reverie pin at this
  frontier = rev `e3e2c96`.
- Probe scripts live in `scripts_probe/` (not part of the shipped suite).

## Method

Three escalating passes, each stricter than the last:

1. **Probe** (`probe_matrix.sh`): every stable + default_only workload run once
   per mode (DEFAULT / VERIFY / CHAOS / CHAOSPMU / RECORD / STRICT), 30s timeout.
   Wide but noisy — a single run of a randomized mode is not trustworthy.
2. **Re-verify** (`reverify.sh`): promotion candidates + contradictions, 60s
   timeout (matching the shipped tests), 2 reps.
3. **Stability + real-harness gate** (`stability.sh`, then
   `record_realtest.sh` / `chaos_realtest.sh`): 5 isolated reps, then the actual
   `cargo test` harness x3. **Only the real harness is authoritative** — see the
   methodology note below.

### Methodology note (why the real harness matters)

The shipped record test asserts the hermit **process exit status**, not just the
`"Success: replay matched recording."` string. Several clone/socket/network
guests print that success line and *then* SIGSEGV on teardown inside
`reverie_process::clone::clone_with_stack::callback` (reverie rev `e3e2c96`),
yielding a non-zero exit → the real test fails even though replay logically
matched. A grep-for-Success check is therefore too lenient for record mode; the
final record set was gated on `cargo test` exit status, x3.

## What was promoted (green in the real `cargo test` harness)

### `hermit_modes.rs` — `buck_chaos_tests!` (+22)

`run --verify --chaos --base-env=empty --preemption-timeout=1000000`, 3/3 in the
real harness:

getcpu, memory_pressure, print_memaddrs, printf_with_threads,
sigtimedwait_no_timeout, sigtimedwait_timeout_0s, sigtimedwait_timeout_1s,
sysinfo_uptime, thread_exhaustion, lit_hello_world_c, lit_networking,
lit_hello_world_rust, rust_stack_ptr, rust_heap_ptrs, rust_rdtsc,
rustbin_clock_gettime, rustbin_exit_group, rustbin_futex_timeout,
rustbin_futex_wait_child, rustbin_nanosleep, rustbin_socketpair,
rustbin_thread_random

(Existing buck_chaos set was 8; now 30.)

### `record_replay.rs` — record/replay (+8)

`record start --verify` then replay, 3/3 in the real harness:

- New C set (`EXPANDED_C_RECORD_WORKLOADS`, `expanded_c_record_replay_matrix`):
  c_getcpu, c_hello_alarm, c_memory_pressure, c_sigtimedwait_no_timeout,
  c_sysinfo_uptime, c_thread_exhaustion
- New Cargo guests (`CARGO_RECORD_GUESTS` / `record_replay_tests!`):
  rustbin_clock_gettime, rustbin_futex_and_print

(Existing record set was 6 C + 15 Cargo = 21; now 12 C + 17 Cargo = 29.)

## What was evaluated and REJECTED (kept honest, not promoted)

| Workload | Mode | Why rejected |
| --- | --- | --- |
| rustbin_socketpair | record | prints Success then SIGSEGV on teardown (reverie clone, e3e2c96); 0/3 real harness |
| rustbin_bind_connect_race | record | same clone SIGSEGV; 0/3 |
| rustbin_network_hello_world | record | same clone SIGSEGV; 0/3 |
| rustbin_interrogate_tty | record | same clone SIGSEGV; 0/3 (also fails DEFAULT run) |
| rustbin_sched_yield | chaos | flaky: probe PASS at 30s was a fluke, 0/2 at 60s |

Note `rustbin_socketpair` is fine under **chaos** (3/3) but not under **record**
— the SIGSEGV is a record-teardown path, so chaos does not hit it.

## Pre-existing RED tests on this frontier/reverie pin (NOT introduced here)

These were already in the committed suite and fail on this worktree's reverie
pin. They are out of scope for this task (removing shipped tests risks masking
real coverage that may pass on a different reverie pin — cf. the reverie-fork
divergence). Documented, not modified:

| Test | Mode | Symptom |
| --- | --- | --- |
| `record_rs_pipe_basics` | record | SIGSEGV in reverie clone_with_stack after "Success" (e3e2c96) |
| `record_rs_poll_spin` | record | same clone SIGSEGV |
| `chaos_buck_nanosleep_parallel` | chaos | times out >90s under chaos+verify+PMU (preemption single-step) |
| `chaos_buck_mem_race` (`rust_mem_race`) | chaos | times out under chaos+verify+PMU |

## Note on `--strict`

`--strict` fail-closes on unsupported syscalls. These micro-workloads directly
invoke rare syscalls (getpid/uname/sysinfo/…) that Detcore does not emulate, so
strict blocks them with ENOSYS. Only the two `nostdlib` guests that avoid
unsupported syscalls (`minimal_hello`, `pread64_nostdlib`) pass `--strict
--verify`. Strict is therefore **not** a useful expansion axis for these
micro-tests; the productive axes are chaos (PMU) and record/replay. This matches
the known result that `--strict` is meaningful for real applications, not for
syscall-probing micro-tests.

## Raw data

- `scripts_probe/matrix_raw_run1.txt` — full 6-mode probe (53 workloads).
- `scripts_probe/reverify_out.txt` — 60s x2 re-verify.
- `scripts_probe/stability_out.txt` — 5-rep isolated gate.
- `scripts_probe/record_realtest_out.txt` — record via real cargo harness x3.
- `scripts_probe/chaos_realtest_out.txt` — chaos via real cargo harness x3.
