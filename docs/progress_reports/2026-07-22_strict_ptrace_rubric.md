# Hermit Progress Report

Generated: **2026-07-22T18:34:28Z**

This is generated evidence, not an estimate. Regenerate it with
[`.llms/skills/progress-rubric.md`](../../.llms/skills/progress-rubric.md) and
[`scripts/progress-report.sh`](../../scripts/progress-report.sh). Raw case results are in
[`2026-07-22_strict_ptrace_rubric.tsv`](2026-07-22_strict_ptrace_rubric.tsv); command logs for this run are in `/tmp/hermit-progress-artifacts-final`.

## Measurement Contract

- Cells are **passed/attempted named cases**. `-` means no runnable case was present; ignored,
  missing-dependency, and absent-target cases are recorded as `SKIP` in the TSV and excluded from
  attempted totals.
- Strict basic-binary cases require Hermit's built-in normalized `--verify` result plus an
  independent workload-marker run. This rejects a backend that exits zero without running the
  requested program while excluding timestamped tool diagnostics from guest determinism.
- C and C++ share **C/C++**, as requested, because their guest-visible syscall surface is the
  relevant unit here.
- Hardware/environment failures remain failures. They are not converted into passes.

## Environment

- Host: `Linux devbig030.atn3.facebook.com 6.13.2-0_fbk13_hardened_0_g02230262e956 #1 SMP Mon Mar 23 09:06:12 PDT 2026 x86_64 x86_64 x86_64 GNU/Linux`
- Main: `bf00a979a70d3e9548d48a9d6fbaea97bce321de` from `/home/newton/work/dev-hermit/worktrees/progress-main-final`
- Frontier: `e7fbcc9aeabddb73da5c34529e636df8c46b3093` from `/home/newton/work/dev-hermit/worktrees/progress-frontier-final`
- Main CI: Rust | completed/success | bf00a97 | 2026-07-22T18:12:31Z | https://github.com/rrnewton/hermit/actions/runs/29945668651
- Frontier CI: no run found

## Branch: main

#### 1. `hermit run` strict determinism (ptrace)

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **151/152** | 21/21 | 124/124 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 1/1 |
| Basic system binaries | 9/10 | 2/2 | 1/1 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 1/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | 113/113 | - | 113/113 | - | - | - | - | - | - | - |
| Integration tests | 29/29 | 19/19 | 10/10 | - | - | - | - | - | - | - |

#### 2. `hermit run --backend dbi`

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **0/10** | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| Basic system binaries | 0/10 | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | - | - | - | - | - | - | - | - | - | - |
| Integration tests | - | - | - | - | - | - | - | - | - | - |

#### 3. `hermit run --backend kvm`

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **0/10** | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| Basic system binaries | 0/10 | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | - | - | - | - | - | - | - | - | - | - |
| Integration tests | - | - | - | - | - | - | - | - | - | - |

#### 4. Record/replay

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **21/26** | 1/1 | 18/18 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 1/1 |
| Basic system binaries | 4/9 | 1/1 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 1/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | - | - | - | - | - | - | - | - | - | - |
| Integration tests | 17/17 | - | 17/17 | - | - | - | - | - | - | - |

### 5. Chaos mode tests

Result: **2/2**

| Test | Language | Status | Evidence |
|---|---|---|---|
| `chaos_mode_matrix::chaos_mode_matrix` | Rust | PASS | `/tmp/hermit-progress-artifacts-final/main-chaos-chaos_mode_matrix` |
| `hello_race_chaos_verify::hello_race_chaos_verify` | Rust | PASS | `/tmp/hermit-progress-artifacts-final/main-chaos-hello_race_chaos_verify` |
| `fast_chaos_matrix::fast_chaos_matrix` | Rust | SKIP | `/tmp/hermit-progress-artifacts-final/main-chaos-fast_chaos_matrix` |

### 6. Debugger attachment tests

Result: **1/1**

| Test | Language | Status | Evidence |
|---|---|---|---|
| `debugger_record_replay` | Other | PASS | `/tmp/hermit-progress-artifacts-final/main-debugger-debugger_record_replay` |

### 7. Schedule bisection examples

Result: **-**

| Test | Language | Status | Evidence |
|---|---|---|---|
| `schedule_bisect` | Rust | SKIP | `NA` |

## Branch: frontier

#### 1. `hermit run` strict determinism (ptrace)

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **320/411** | 167/245 | 151/156 | 1/3 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 1/2 |
| Basic system binaries | 5/10 | 2/2 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 1/1 |
| rr test suite | 145/213 | 145/213 | - | - | - | - | - | - | - | - |
| OSS full apps | 1/6 | 1/5 | - | 0/1 | - | - | - | - | - | - |
| Unit tests | 126/128 | - | 126/128 | - | - | - | - | - | - | - |
| Integration tests | 43/54 | 19/25 | 24/27 | 0/1 | - | - | - | - | - | 0/1 |

#### 2. `hermit run --backend dbi`

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **0/10** | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| Basic system binaries | 0/10 | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | - | - | - | - | - | - | - | - | - | - |
| Integration tests | - | - | - | - | - | - | - | - | - | - |

#### 3. `hermit run --backend kvm`

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **0/10** | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| Basic system binaries | 0/10 | 0/2 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | - | - | - | - | - | - | - | - | - | - |
| Integration tests | - | - | - | - | - | - | - | - | - | - |

#### 4. Record/replay

| Category | Total | C/C++ | Rust | Python | Java | Go | Ruby | OCaml | Node.js | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Grand total** | **23/31** | 1/1 | 20/23 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 1/1 |
| Basic system binaries | 3/9 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 1/1 |
| rr test suite | - | - | - | - | - | - | - | - | - | - |
| OSS full apps | - | - | - | - | - | - | - | - | - | - |
| Unit tests | - | - | - | - | - | - | - | - | - | - |
| Integration tests | 20/22 | - | 20/22 | - | - | - | - | - | - | - |

### 5. Chaos mode tests

Result: **3/3**

| Test | Language | Status | Evidence |
|---|---|---|---|
| `chaos_mode_matrix::chaos_mode_matrix` | Rust | PASS | `/tmp/hermit-progress-artifacts-final/frontier-chaos-chaos_mode_matrix` |
| `hello_race_chaos_verify::hello_race_chaos_verify` | Rust | PASS | `/tmp/hermit-progress-artifacts-final/frontier-chaos-hello_race_chaos_verify` |
| `fast_chaos_matrix::fast_chaos_matrix` | Rust | PASS | `/tmp/hermit-progress-artifacts-final/frontier-chaos-fast_chaos_matrix` |

### 6. Debugger attachment tests

Result: **1/1**

| Test | Language | Status | Evidence |
|---|---|---|---|
| `debugger_record_replay` | Other | PASS | `/tmp/hermit-progress-artifacts-final/frontier-debugger-debugger_record_replay` |

### 7. Schedule bisection examples

Result: **1/1**

| Test | Language | Status | Evidence |
|---|---|---|---|
| `schedule_bisect::schedule_bisect_localizes_publish_ordering_race` | Rust | PASS | `/tmp/hermit-progress-artifacts-final/frontier-bisection-schedule_bisect` |

## Observed Failures

| Branch | Mode | Backend | Category | Language | Cases | Example | Exit | Evidence |
|---|---|---|---|---|---:|---|---:|---|
| frontier | strict | dbi | basic_system_binaries | Java | 1 | `java_version` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-java_version` |
| main | strict | dbi | basic_system_binaries | Node.js | 1 | `node_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-node_hello` |
| frontier | strict | dbi | basic_system_binaries | Rust | 1 | `rustc_version` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-rustc_version` |
| frontier | strict | kvm | basic_system_binaries | C/C++ | 1 | `c_ls` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-c_ls` |
| frontier | strict | dbi | basic_system_binaries | Go | 1 | `go_version` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-go_version` |
| frontier | strict | dbi | basic_system_binaries | Python | 1 | `python_hello` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-python_hello` |
| frontier | strict | ptrace | integration_tests | Rust | 1 | `no_silent_skips::test_control_sources_have_no_silent_skip_markers` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-no_silent_skips` |
| frontier | strict | ptrace | basic_system_binaries | Java | 1 | `java_version` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-ptrace-java_version` |
| frontier | strict | ptrace | integration_tests | C/C++ | 1 | `clock_determinism::strict_mode_eliminates_native_clock_nondeterminism` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-clock_determinism` |
| main | record_replay | ptrace | basic_system_binaries | Java | 1 | `java_version` | 124 | `/tmp/hermit-progress-artifacts-final/main-record-java_version` |
| frontier | strict | ptrace | integration_tests | C/C++ | 2 | `fork_exec_determinism::fork_exec_inherits_fd_environment_and_cwd` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-fork_exec_determinism` |
| main | strict | dbi | basic_system_binaries | Go | 1 | `go_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-go_version` |
| frontier | record_replay | ptrace | basic_system_binaries | Java | 1 | `java_version` | 124 | `/tmp/hermit-progress-artifacts-final/frontier-record-java_version` |
| main | strict | dbi | basic_system_binaries | Ruby | 1 | `ruby_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-ruby_hello` |
| main | strict | kvm | basic_system_binaries | Rust | 1 | `rustc_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-rustc_version` |
| frontier | strict | dbi | basic_system_binaries | Node.js | 1 | `node_hello` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-node_hello` |
| frontier | record_replay | ptrace | basic_system_binaries | Rust | 1 | `rustc_version` | 1 | `/tmp/hermit-progress-artifacts-final/frontier-record-rustc_version` |
| frontier | strict | kvm | basic_system_binaries | C/C++ | 1 | `cpp_gpp` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-cpp_gpp` |
| frontier | strict | kvm | basic_system_binaries | Node.js | 1 | `node_hello` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-node_hello` |
| frontier | strict | ptrace | oss_full_apps | C/C++ | 1 | `sqlite_veryquick::sqlite_fast_subset_is_deterministic_under_strict_hermit` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-sqlite_veryquick` |
| frontier | strict | ptrace | basic_system_binaries | Ruby | 1 | `ruby_hello` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-ptrace-ruby_hello` |
| main | strict | dbi | basic_system_binaries | Python | 1 | `python_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-python_hello` |
| main | record_replay | ptrace | basic_system_binaries | Node.js | 1 | `node_hello` | 1 | `/tmp/hermit-progress-artifacts-final/main-record-node_hello` |
| main | strict | dbi | basic_system_binaries | Other | 1 | `shell_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-shell_hello` |
| frontier | record_replay | ptrace | integration_tests | Rust | 2 | `record_replay::record_rs_pipe_basics` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-record_replay-record_replay` |
| frontier | strict | ptrace | basic_system_binaries | OCaml | 1 | `ocaml_version` | 127/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-ptrace-ocaml_version` |
| frontier | strict | kvm | basic_system_binaries | Ruby | 1 | `ruby_hello` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-ruby_hello` |
| frontier | strict | ptrace | unit_tests | Rust | 2 | `workspace_libs::tests::default_and_available_backends_reflect_host_probes` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-workspace_libs` |
| main | strict | kvm | basic_system_binaries | Other | 1 | `shell_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-shell_hello` |
| frontier | strict | ptrace | oss_full_apps | C/C++ | 1 | `compression::compression_tools_are_deterministic_under_strict_hermit` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-compression` |
| main | strict | kvm | basic_system_binaries | Go | 1 | `go_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-go_version` |
| frontier | record_replay | ptrace | basic_system_binaries | OCaml | 1 | `ocaml_version` | 1 | `/tmp/hermit-progress-artifacts-final/frontier-record-ocaml_version` |
| frontier | strict | ptrace | oss_full_apps | Python | 1 | `python_stdlib::strict_python_stdlib_is_deterministic` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-python_stdlib` |
| frontier | strict | dbi | basic_system_binaries | Ruby | 1 | `ruby_hello` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-ruby_hello` |
| frontier | strict | ptrace | integration_tests | Python | 1 | `hashseed_determinism::python_set_order_nondeterministic_natively_deterministic_under_hermit` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-hashseed_determinism` |
| main | strict | dbi | basic_system_binaries | Rust | 1 | `rustc_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-rustc_version` |
| main | record_replay | ptrace | basic_system_binaries | Go | 1 | `go_version` | 1 | `/tmp/hermit-progress-artifacts-final/main-record-go_version` |
| frontier | strict | kvm | basic_system_binaries | OCaml | 1 | `ocaml_version` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-ocaml_version` |
| frontier | strict | kvm | basic_system_binaries | Java | 1 | `java_version` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-java_version` |
| main | record_replay | ptrace | basic_system_binaries | Ruby | 1 | `ruby_hello` | 1 | `/tmp/hermit-progress-artifacts-final/main-record-ruby_hello` |
| main | strict | dbi | basic_system_binaries | C/C++ | 1 | `cpp_gpp` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-cpp_gpp` |
| frontier | strict | ptrace | integration_tests | Rust | 2 | `cli::backend_accepted_in_global_position` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-cli` |
| frontier | strict | ptrace | basic_system_binaries | Node.js | 1 | `node_hello` | 0/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-ptrace-node_hello` |
| main | strict | kvm | basic_system_binaries | OCaml | 1 | `ocaml_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-ocaml_version` |
| frontier | strict | kvm | basic_system_binaries | Rust | 1 | `rustc_version` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-rustc_version` |
| main | strict | kvm | basic_system_binaries | Python | 1 | `python_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-python_hello` |
| main | strict | kvm | basic_system_binaries | Node.js | 1 | `node_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-node_hello` |
| main | strict | kvm | basic_system_binaries | Ruby | 1 | `ruby_hello` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-ruby_hello` |
| frontier | strict | ptrace | basic_system_binaries | Go | 1 | `go_version` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-ptrace-go_version` |
| frontier | strict | ptrace | integration_tests | Other | 1 | `integration_matrix::integration_matrix` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-integration_matrix` |
| frontier | strict | kvm | basic_system_binaries | Go | 1 | `go_version` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-go_version` |
| frontier | strict | ptrace | oss_full_apps | C/C++ | 2 | `redis_strict::redis_persistence_restart_is_deterministic_under_strict_hermit` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-redis_strict` |
| main | strict | kvm | basic_system_binaries | C/C++ | 1 | `c_ls` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-c_ls` |
| main | strict | dbi | basic_system_binaries | C/C++ | 1 | `c_ls` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-c_ls` |
| main | record_replay | ptrace | basic_system_binaries | OCaml | 1 | `ocaml_version` | 1 | `/tmp/hermit-progress-artifacts-final/main-record-ocaml_version` |
| main | strict | kvm | basic_system_binaries | C/C++ | 1 | `cpp_gpp` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-cpp_gpp` |
| frontier | strict | dbi | basic_system_binaries | OCaml | 1 | `ocaml_version` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-ocaml_version` |
| main | strict | kvm | basic_system_binaries | Java | 1 | `java_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-kvm-java_version` |
| main | strict | dbi | basic_system_binaries | OCaml | 1 | `ocaml_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-ocaml_version` |
| frontier | strict | kvm | basic_system_binaries | Python | 1 | `python_hello` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-python_hello` |
| frontier | strict | ptrace | integration_tests | C/C++ | 1 | `signal_determinism::harness` | 124 | `/tmp/hermit-progress-artifacts-final/frontier-strict-signal_determinism` |
| frontier | record_replay | ptrace | basic_system_binaries | Go | 1 | `go_version` | 1 | `/tmp/hermit-progress-artifacts-final/frontier-record-go_version` |
| main | strict | dbi | basic_system_binaries | Java | 1 | `java_version` | 2/2 | `/tmp/hermit-progress-artifacts-final/main-strict-dbi-java_version` |
| main | strict | ptrace | basic_system_binaries | Ruby | 1 | `ruby_hello` | 1/1 | `/tmp/hermit-progress-artifacts-final/main-strict-ptrace-ruby_hello` |
| frontier | record_replay | ptrace | basic_system_binaries | Node.js | 1 | `node_hello` | 1 | `/tmp/hermit-progress-artifacts-final/frontier-record-node_hello` |
| frontier | strict | dbi | basic_system_binaries | C/C++ | 1 | `c_ls` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-c_ls` |
| frontier | strict | ptrace | integration_tests | C/C++ | 1 | `epoll_determinism::mixed_fd_readiness_is_deterministic` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-epoll_determinism` |
| frontier | record_replay | ptrace | basic_system_binaries | Ruby | 1 | `ruby_hello` | 1 | `/tmp/hermit-progress-artifacts-final/frontier-record-ruby_hello` |
| frontier | strict | ptrace | rr_test_suite | C/C++ | 68 | `rr_suite::rr_fatal_init_signal` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-rr_suite` |
| frontier | strict | dbi | basic_system_binaries | Other | 1 | `shell_hello` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-shell_hello` |
| frontier | strict | dbi | basic_system_binaries | C/C++ | 1 | `cpp_gpp` | 1/1 | `/tmp/hermit-progress-artifacts-final/frontier-strict-dbi-cpp_gpp` |
| frontier | strict | ptrace | integration_tests | C/C++ | 1 | `fp_reduction_determinism::strict_parallel_fp_reduction_is_bit_identical` | 101 | `/tmp/hermit-progress-artifacts-final/frontier-strict-fp_reduction_determinism` |
| frontier | strict | kvm | basic_system_binaries | Other | 1 | `shell_hello` | 0/0 | `/tmp/hermit-progress-artifacts-final/frontier-strict-kvm-shell_hello` |

## Skipped Coverage

| Branch | Mode | Category | Language | Cases | Example | Reason/command |
|---|---|---|---|---:|---|---|
| main | strict | integration_tests | Rust | 2 | `thread_scheduling_fairness` | test target absent on this branch |
| main | strict | integration_tests | Python | 1 | `hashseed_determinism` | test target absent on this branch |
| main | strict | integration_tests | Other | 1 | `integration_matrix` | test target absent on this branch |
| main | chaos | chaos | Rust | 1 | `fast_chaos_matrix::fast_chaos_matrix` | timeout 600s cargo test -p hermit --test stress_suite fast_chaos_matrix -- --exact  |
| frontier | strict | oss_full_apps | C/C++ | 1 | `leveldb` | LEVELDB_BUILD_DIR or test target unavailable |
| main | strict | integration_tests | C/C++ | 2 | `fork_exec_determinism` | test target absent on this branch |
| main | strict | rr_test_suite | C/C++ | 1 | `rr_suite` | rr target or initialized third-party/rr submodule unavailable |
| frontier | strict | oss_full_apps | C/C++ | 1 | `redis_strict::redis_source_build_and_extended_suite_under_strict_hermit` | timeout 300s cargo test -p hermit --test redis_strict -- --test-threads=1  |
| main | strict | oss_full_apps | C/C++ | 1 | `leveldb` | LEVELDB_BUILD_DIR or test target unavailable |
| main | strict | oss_full_apps | Python | 1 | `python_stdlib` | test target absent on this branch |
| main | strict | oss_full_apps | C/C++ | 3 | `compression` | test target absent on this branch |
| main | bisection | schedule_bisection | Rust | 1 | `schedule_bisect` | test target absent on this branch |
