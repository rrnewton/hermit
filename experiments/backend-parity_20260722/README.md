# Hermit backend parity matrix

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBI), and KVM backends. `matrix.tsv` is the ratchet: changing a pair
from `gap` to `pass` makes `run_matrix.py` enforce it on every subsequent run.
A `gap` must have a concrete implementation reason.

## Current ratchet

| Backend | Passing pairs | Parity vs ptrace |
| --- | ---: | ---: |
| ptrace | 14/14 | 100% |
| DBI | 11/14 | 78.6% |
| KVM | 1/14 | 7.1% |

The DBI matrix covers the file-I/O, memory-management, process-lifecycle, and
signal syscall batches with focused C guests. The process guest exercises
`fork`, `vfork`, `clone`, `execve`, `wait4`, `waitid`, `exit`, and
`exit_group`. Thread lifecycle remains a documented gap on current `main`:
the pending batch-4 scheduling bridge is not landed, and the pthread probe
currently terminates with `SIGSEGV` under DynamoRIO.

The task's pre-existing DBI-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 11/14 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.

KVM's single passing pair is the built-in hello/write VM-exit path. The current
KVM prototype does not load the requested Linux ELF, so treating `/bin/true`
returning zero as a pass would be a false positive. Its CPUID policy is covered
inside `reverie-kvm`, but it cannot yet execute this suite's CPUID probe ELF.

## Matrix

| Test | ptrace | DBI | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass | pass | pass |
| `argument_forwarding` | pass | pass | gap |
| `exit_zero` | pass | pass | gap |
| `exit_status` | pass | pass | gap |
| `file_read` | pass | pass | gap |
| `file_io_batch` | pass | pass | gap |
| `memory_batch` | pass | pass | gap |
| `process_batch` | pass | pass | gap |
| `signal_batch` | pass | pass | gap |
| `pthread_lifecycle` | pass | gap | gap |
| `cpuid_policy` | pass | pass | gap |
| `virtual_clock` | pass | pass | gap |
| `random_sources` | pass | gap | gap |
| `virtual_pid` | pass | gap | gap |

The authoritative reasons live in `matrix.tsv`, next to the status they
justify. The runner executes each passing pair three times and checks exit
status and stdout. `--strict-verify` pairs every zero-exit functional
invocation with an
L2 verification invocation; ptrace verification intentionally suppresses guest
stdout, so keeping both phases prevents a false behavioral pass. The expected
nonzero `exit_status` case runs three strict L1 invocations because Hermit
verification intentionally stops after a nonzero first run.

## Running

Validate the checked-in matrix without backend prerequisites:

```bash
python3 experiments/backend-parity_20260722/run_matrix.py --check
```

Build release Hermit and run one backend's focused validation profile:

```bash
./validate.sh --backend ptrace --no-label-pr
./validate.sh --backend dbi --no-label-pr
```

The equivalent direct DBI command is:

```bash
python3 experiments/backend-parity_20260722/run_matrix.py \
  --backend dbi \
  --hermit target/release/hermit \
  --strict-verify \
  --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates.
Use `--output /tmp/backend-parity.tsv` to retain machine-readable observations.
`BLOCKED` means a required host capability or runtime artifact was absent; it
does not change the checked-in pass/gap claim. Timed-out commands run in their
own process groups so DynamoRIO descendants cannot survive the runner.
