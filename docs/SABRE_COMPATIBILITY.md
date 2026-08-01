# SaBRe backend compatibility

SaBRe is an experimental Linux x86-64 execution backend for Hermit `run`. It
loads the shared Detcore implementation into a SaBRe plugin while a Hermit
coordinator owns Detcore's global state. It is useful for measured workloads,
but it is not a drop-in replacement for the ptrace backend.

This document describes the post-0.2 work-ahead envelope. The executable test
manifests are the source of truth: a SaBRe entry in `backends_enabled` means the
named cell passed both SaBRe strict verification and its stated semantic
oracle. An exclusion remains a support gap, not an implied fallback to ptrace.

## Build and run

SaBRe is behind the non-default `third-party-backends` Cargo feature and needs
the staged loader plus `libdetcore_sabre.so` beside the Hermit executable:

```bash
cargo build --release --locked -p hermit \
  --features third-party-backends -p detcore-sabre
HERMIT_INSTALL_FORCE_RESTAGE=local-sabre \
  cargo build --release --locked -p hermit-install

target/release/hermit run --backend sabre --strict --verify -- /bin/echo hello
```

An explicit SaBRe request fails closed if the feature or artifacts are absent.
Hermit does not silently substitute ptrace.

## What the backend actually executes

The CLI path is:

```text
run_with_backend_inner(Backend::Sabre)
  -> run_sabre
  -> detcore::GlobalState in the Hermit coordinator
  -> detcore-sabre::Plugin implementing reverie_sabre::Tool
  -> RemoteReverieAdapter<Detcore> / SabreGuest
  -> shared Detcore syscall and scheduler logic
```

This path is visible in INFO logs as
`launching Detcore guest through SaBRe with coordinator RPC`, followed by
`detcore::scheduler` commit turns. SaBRe uses a plugin/guest adapter rather
than a generic `impl reverie::Backend`; the architectural difference does not
create a second determinism engine.

## Measured strict-verify envelope

Snapshot:

- Runtime implementation base: Hermit
  `0ca0dec256fd484e238b475a031a5c2d482eeba8` (version 0.2.0), Reverie dependency
  `adc147342f34754b449b9a24174aca3ac3a2e16b`.
- SaBRe loader: `80883b80a74d9c649419bdacc97dfd146baa34df`,
  SHA-256 `cd0b75ed6f585a2447675a9b74577a3ec643489615a3549f9e95ca4705893418`.
- Host: Linux `6.18.39-0_fbk0_hardened_0_ga43d5727b443`, AMD EPYC 9D85,
  `perf_event_paranoid=1`.
- Toolchain: `rustc 1.99.0-nightly (26ae60a9e 2026-07-28)`.
- Level: L2 (`run --backend sabre --strict --verify`). The portable corpus
  uses `--no-virtualize-cpuid --max-timeslice=disabled`; the standalone
  `/bin/echo`, `/bin/true`, and `/bin/cat /dev/null` probes pass L2 without
  relaxations.
- Log level: INFO for verification. Every cell was bounded by its manifest
  timeout. `race.sh` was not run.

The baseline sweep's ptrace strict-verify plan had 194 cells. The current plan
has 199 after later ptrace-only cells landed. Before the baseline sweep, SaBRe
was enabled for 22 (11.3%). That sweep evaluated 157 previously disabled C
candidates:

| Result | Cells | Meaning |
| --- | ---: | --- |
| SaBRe L2 and ptrace exit/stdout parity | 109 | Enabled by this ratchet |
| SaBRe L2, but ptrace output differs | 18 | Remains disabled |
| SaBRe L2 failed or timed out | 30 | Remains disabled |

The resulting plan enables SaBRe for 131/194 cells (67.5%): seven blocking CI
cells and 124 manual cells. This meets the B3 corpus-count threshold (at least
50% of the ptrace strict-verify corpus). It does not establish B4, L3 memory
determinism, L4 stress hardening, or support for every workload in a subsystem.

The 109 newly enabled cells are grouped as follows:

| Manifest bucket | New cells |
| --- | ---: |
| `c-programs` | 99 |
| `determinism-stress-c` | 7 |
| `backend-parity-c` | 1 |
| `bin-c` | 1 |
| `chaos-c` verify mode | 1 |

The exact allowlist is available with:

```bash
cargo run --locked --quiet -p hermit-manifest-plan -- --format json \
  | jq '.[] | select(.backend == "sabre" and .mode == "verify")'
```

Representative coverage includes dynamically linked process/thread lifecycle,
file and procfs metadata, memory mapping, timers, PIDFD, poll, netlink and UNIX
socket autobind, TCP info, syscall-refusal semantics, pipes, fork trees, shared
mappings, and signal ordering. These are probe-specific claims; for example,
some fork and signal probes pass while other probes in those categories do not.

The non-gated static-ELF increment was validated at Hermit
`d09bd347df9986eab05d78572547bc382af57d6a`, Reverie
`47544aecfcb3ec92fa6fb5d92bd5c75ac72c0a98`, and SaBRe
`df1839a129d93b69f47a819a3769c8cbb0b4ec60` (loader SHA-256
`f901b227eb9c1ff28cda292cbd8e9c8308fcfdaa57aa4bb7fadf610990b03812`).
The exact `hello-nostdlib` and `pread64-nostdlib` manifest cells pass L2 with
the SaBRe backend, INFO logging, and the portable
`--no-virtualize-cpuid --max-timeslice=disabled` profile. The same cells pass
ptrace L2. Separate direct strict runs produced byte-identical ptrace/SaBRe
guest stdout: 14-byte `Hello, World!\n` for `hello-nostdlib` and empty stdout
for `pread64-nostdlib`; both backends exited zero. The executable plan is now
133/199 SaBRe verify cells (66.8%, B3), up from 131/199 (65.8%) on the same
plan: seven blocking CI cells and 126 manual cells. This is not a full-corpus
rerun or a B4/full-parity claim.

## Known gaps

The following 18 cells are deterministic inside SaBRe but do not match ptrace
guest output, so they remain disabled:

```text
backend-parity-c/pid-probe
c-programs/dbi-pid-virtualization
c-programs/print-memaddrs
c-programs/proc-fdinfo
c-programs/random-sources
c-programs/setitimer-determinism
c-programs/sigtimedwait-timeout-1s
c-programs/socket-cookie-tcp
c-programs/socket-cookie-udp
c-programs/socket-cookie-unix
c-programs/socket-timestamp-edge-cases
c-programs/socket-timestamp-timespec
c-programs/socket-timestamp-timeval
c-programs/sysinfo
c-programs/sysinfo-uptime
c-programs/wait-on-child
debugger-c/debuggee
determinism-stress-c/pid-tid
```

The following 28 candidates fail SaBRe strict verification or its timeout and
remain disabled:

```text
backend-parity-c/cpuid-probe
bin-c/robust-futex-test
c-programs/arch-prctl-determinism
c-programs/clone
c-programs/dbi-unsupported-syscall
c-programs/epoll-determinism
c-programs/fp-reduction-nondeterminism
c-programs/ipc-determinism
c-programs/liteinst-advanced
c-programs/nanosleep-threads-simple
c-programs/pselect6-simulation
c-programs/racewrite-nostdlib
c-programs/record-replay-file-state
c-programs/record-replay-lseek-seek-cur
c-programs/resource-determinism
c-programs/signal-determinism
c-programs/sigpipe-siginfo
c-programs/sigtimedwait-no-timeout
c-programs/socket-ioctl-timestamp
c-programs/thread-sync-determinism
c-programs/vforkexec
c-programs/writev-determinism
determinism-stress-c/thread-contention
shared-futex-c/qemu-exec-init
shared-futex-c/qemu-hello
shared-futex-c/qemu-init
shared-futex-c/qemu-net-init
util-c/pmu-skid
```

Additional backend-wide limits:

- GNU `patch` reaches `getrandom` through glibc at a libc site that the SaBRe
  syscall rewriter can miss. The plugin detours that libc function through
  Detcore; the canonical `patch` workload then passed five consecutive strict
  verification probes with matching DETLOG/COMMIT streams. This does not close
  the broader random-source gap: the multithreaded `random-sources` probe still
  produces different ptrace and SaBRe stdout and DETLOG streams and remains
  disabled.
- The exhaustive `relaxed_flag_matrix` integration test is currently a
  ptrace-only cross-product. It exercises getrandom in its observation guest,
  but provides no SaBRe flag-matrix coverage; adding a bounded SaBRe slice is a
  separate qualification batch.
- SaBRe supports deterministic `run` and the narrow SaBRe `strace` command;
  record/replay and chaos scheduling are unsupported.
- `race.sh` is excluded. SaBRe does not serialize arbitrary guest instructions
  between callbacks, so a callback-only result would not prove schedule parity.
- CPUID and RDTSCP are not fully intercepted. The clock-determinism cell remains
  disabled because raw host TSC can leak through RDTSCP.
- Continuous virtual time is deterministic within each backend, but the ptrace
  and SaBRe clock trajectories are not yet identical.
- The two single-threaded, freestanding x86-64 static probes now pass, but
  `racewrite-nostdlib` remains disabled. This does not qualify threaded static
  guests, static glibc, or other architectures.
- Process and thread support is selective: fork-tree and several lifecycle
  probes pass, while raw clone, vfork/exec, robust-futex, and some contention
  probes fail or time out.

## Reproducing the gates

Validate manifest policy and run the blocking SaBRe cells:

```bash
./ci/test_harness.sh validate
./ci/test_harness.sh run --lane portable --backend sabre --ci-only
```

Run a manual enabled cell with its exact ID:

```bash
./ci/test_harness.sh run --include-manual --mode verify \
  --backend sabre --test c-programs/syscall-file-io
```

All Hermit namespace runs require a host that permits the required user, PID,
mount, and network namespaces.
