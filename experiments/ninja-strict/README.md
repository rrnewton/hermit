# Ninja strict-mode determinism

This experiment builds Ninja's upstream C++ test binary and runs it twice
under Hermit strict mode. Ninja is pinned to v1.13.1 commit
`79feac0f3e3bc9da9effc586cd5fea41e7550051`; GoogleTest is pinned to v1.16.0
with its source archive verified by SHA-256.

## Run

From the repository root:

```sh
./experiments/ninja-strict/run.sh
```

The runner clones Ninja and downloads GoogleTest through `with-proxy`, builds
`ninja`, `ninja_test`, and release Hermit, and leaves generated sources and
binaries under `target/ninja-strict/`. It first requires the complete native
Ninja suite to pass. It then executes the supported strict-mode test set twice
and compares stdout, stderr, and exit status byte-for-byte.

The runner applies `ninja-test-in-process-cleanup.patch` before building.
Ninja's otherwise in-process disk and deps-log fixtures call `system("rm -rf")`
from their shared teardown helper. Strict runs set
`NINJA_TEST_KEEP_TEMP_DIRS=1` to skip that subprocess because every execution
already has a private Hermit `--tmp` tree. The native control leaves upstream
cleanup behavior enabled.

Use existing patched build artifacts with:

```sh
./experiments/ninja-strict/run.sh \
  --skip-build \
  --source /path/to/ninja \
  --build /path/to/ninja-build \
  --gtest-source /path/to/googletest-1.16.0 \
  --hermit ./target/release/hermit \
  --output /tmp/ninja-evidence
```

The default strict command is equivalent to:

```text
hermit --log=error run --strict --base-env=minimal --env=LC_ALL=C \
  --env=NINJA_TEST_KEEP_TEMP_DIRS=1 --tmp=<isolated run directory> \
  -- ninja_test --gtest_color=no --gtest_filter=-SubprocessTest.*
```

The default two-run test takes seconds once the binaries exist. The external
clone and C++ build are intentionally an experiment setup step rather than a
network-dependent Cargo test, so no slow `#[ignore]` test is added to the Rust
suite.

## Recorded result

`evidence_20260722/` was collected while revising Hermit commit
`d23fc497a1fe95b4f3b2233a5ed12424c093b8c1` on an x86-64 AMD EPYC 9D85
host. The native control passed all 410 tests. Both strict runs passed 397
tests from 30 suites, exited zero with empty stderr, and produced the same
stdout hash:

```text
1bd17d2bc25c54fba1b44f2cea98ec0b183a11aca589b603da30badb82b266e7
```

The strict set includes all 9 `DiskInterfaceTest` cases and all 10
`BuildWithDepsLogTest` cases. Only the 13 `SubprocessTest` cases are
excluded.

## Full-suite blocker

The complete suite does not currently pass under Hermit. `SubprocessTest`
uses `posix_spawn`; glibc enters `clone(CLONE_VM | CLONE_VFORK)`, which
Hermit rejects.

Run the bounded diagnostic with:

```sh
./experiments/ninja-strict/run.sh --skip-build --probe-full
```

The recorded probe times out at `SubprocessTest.BadCommandStderr` after Hermit
reports unsupported `CLONE_VFORK`. A direct-`fork` Ninja 1.6 control exposed
a second scheduler issue: the parent can block on the child pipe before the new
child receives its initial go-ahead. Consequently, merely replacing
`posix_spawn` does not make the full test suite pass.

The 397-test result covers parser, graph, build-plan, disk-interface, depfile,
deps-log, dyndep, log, and other in-process Ninja logic. It deliberately does
not claim child-process determinism. The probe remains in the runner so a
future scheduler fix can convert this limitation into full-suite coverage
without changing the workload.
