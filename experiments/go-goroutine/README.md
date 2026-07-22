# Go goroutine scheduling determinism

This experiment is the `goroutine-channel-order` program in `main.go`.

`NONDET_SOURCE: Go goroutine scheduling.` Thirty-two goroutines wait on a
shared start channel, yield once, and send their IDs to a results channel. The
receive order depends on the Go runtime and host thread schedules. The program
prints that order and its SHA-256 so the integration test can use the dual
assertion pattern:

- native execution must produce at least two unique hashes;
- six `hermit run --strict` executions must produce one hash and byte-identical
  output.

## Observed baseline

Recorded on 2026-07-22:

```text
compiler: go version go1.26.4 (Red Hat 1.26.4-1.el9) linux/amd64
program: goroutine-channel-order
native: 24 runs, 24 unique hashes
strict: 8 runs, 1 unique hash
strict sha256: ea892a10d4bae781a637aac2f4252b99e564e2be8ccec2d22c9cc089ceecd996
```

The host kernel does not provide CPUID faulting, so the strict verification
used `--no-virtualize-cpuid`. PMU preemption was disabled; this test exercises
deterministic syscall and OS-thread scheduling rather than branch-count
preemption.

## Run

```bash
go build -trimpath -o /tmp/goroutine-channel-order ./experiments/go-goroutine/main.go
for run in $(seq 1 24); do /tmp/goroutine-channel-order; done

for run in $(seq 1 8); do
  target/debug/hermit run --strict \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --preemption-timeout=disabled \
    --tmp=/tmp \
    -- /tmp/goroutine-channel-order
done

cargo test -p hermit --test go_goroutine_determinism -- --nocapture
```
