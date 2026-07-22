# rr syscall test suite under Hermit

Hermit reuses the focused syscall edge-case test programs from the
[rr](https://github.com/rr-debugger/rr) record/replay debugger. Each rr test is a
small standalone C program that exercises one syscall or kernel corner case and
`assert()`s the observed behavior. Running them under `hermit run` is a strong
regression guardrail: Hermit must reproduce real kernel semantics deterministically.

This mirrors the fbsource `RR_TEST_TARGETS` set defined in
`hermetic_infra/common/wrap_test_suite.bzl`.

## Layout

- **Sources:** the pinned `third-party/rr` git submodule (upstream commit
  `39e5c18`). Initialize it with:
  ```sh
  git submodule update --init third-party/rr
  ```
- **Harness:** `hermit-cli/tests/rr_suite.rs`. It generates rr's syscall-enum
  headers with rr's own `generate_syscalls.py`, compiles each `src/test/<name>.c`
  with rr's `RR_TEST_FLAGS` (`-D_FILE_OFFSET_BITS=64 -pthread -std=gnu11 -g3 -O0`,
  linked against `-ldl -lrt`), and runs it as:
  ```sh
  hermit run --base-env=minimal --preemption-timeout=80000000 -- <program> [args]
  ```
  asserting the expected exit code. Each invocation uses a unique temporary
  working directory that is removed after the test.

## Running

The programs are ptrace-heavy and depend on PMU branch counters plus working
user/mount namespaces, so they are `#[ignore]`d by default (like the other Hermit
integration suites) and run explicitly:

```sh
cargo test -p hermit --test rr_suite -- --ignored          # all
cargo test -p hermit --test rr_suite -- --ignored rr_hello # one
```

`validate.sh` runs the full suite as its "rr syscall suite" check.

## Coverage

The exported Hermit target set contains **219** rr programs. Every one now has
an executable Rust test in `rr_suite.rs`:

- **214 expected passes** assert their exact exit status.
- **5 expected failures (xfails)** execute in the same CI command and assert the
  documented failure shape.
- **0 omitted or commented-out tests.**

The fbsource `test_arch_prctl` mapping is
`src/test/x86/arch_prctl_x86.c` in the pinned upstream tree. It passes under
Hermit and is included in the 214 normal tests. Special expected-pass cases
carried over from fbsource are:

- `rr_args` runs with `-no --force-syscall-buffer=foo -c 1000 hello`
  (exit 0).
- `rr_pause` expects exit code 1.

The harness has an inventory regression that counts the generated test
declarations and requires exactly 214 passes plus 5 xfails. A normal test fails
on any unexpected status. An xfail also fails if it unexpectedly passes or
changes to an unrecognized failure; this forces the issue and expectation to be
removed when the underlying bug is fixed.

### Expected failures

| rr test | asserted failure | reason | issue |
| --- | --- | --- | --- |
| `rr_rusage` | SIGABRT and `rusage.c:10: !(r->ru_maxrss > 0)` | `getrusage` exposes `ru_maxrss == 0` instead of deterministic Linux-compatible usage | [#114](https://github.com/rrnewton/hermit/issues/114) |
| `rr_sigchld_interrupt_signal` | timeout exit 124 after 10s | SIGCHLD delivery does not make the expected interrupt/restart path progress | [#115](https://github.com/rrnewton/hermit/issues/115) |
| `rr_sigprocmask_in_syscallbuf_sighandler` | timeout exit 124 after 10s | changing a signal mask inside a syscall-buffer signal handler stalls | [#112](https://github.com/rrnewton/hermit/issues/112) |
| `rr_spinlock_priorities` | timeout exit 124 after 10s | priority-sensitive scheduler progress stalls around the userspace spinlock | [#113](https://github.com/rrnewton/hermit/issues/113) |
| `rr_multiple_pending_signals_sequential` | three attempts; each must be exit 0 or timeout 124, with at least one timeout | sequential delivery of multiple pending signals intermittently stops making progress | [#116](https://github.com/rrnewton/hermit/issues/116) |

Known hangs use a 10-second xfail cap so they remain executed without adding
minutes to CI. Normal programs retain the 120-second regression timeout.

The 219 cases are the fbsource `RR_TEST_TARGETS` after its existing
Hermit-specific `wrap_test_suite(exclude=[...])` policy. The roughly 78 tests
excluded by that upstream policy are not members of this exported target set;
changing that policy requires a separate porting decision.

## Refreshing

To advance the pinned rr version, update the submodule and re-triage which
programs pass, then adjust the `rr_test!` list in `hermit-cli/tests/rr_suite.rs`.
