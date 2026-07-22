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

Use existing build artifacts with:

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
  --tmp=<isolated run directory> -- ninja_test --gtest_color=no \
  --gtest_filter=-SubprocessTest.*:DiskInterfaceTest.*:BuildWithDepsLogTest.*
```

The default two-run test takes seconds once the binaries exist. The external
clone and C++ build are intentionally an experiment setup step rather than a
network-dependent Cargo test, so no slow `#[ignore]` test is added to the Rust
suite.

## Recorded result

`evidence_20260722/` was collected from Hermit commit
`d6438c9d5fe1b2076eab3d563b445b96c15f70d7` on an x86-64 AMD EPYC 9D85 host.
The native control passed all 410 tests. Both strict runs passed 378 tests from
28 suites, exited zero with empty stderr, and produced the same stdout hash:

```text
56fbdedcdb8a631bb3929a4a17411db489158f32e07d3fb5164f1a798c5a4b00
```

## Full-suite blocker

The complete suite does not currently pass under Hermit. Its three
child-process fixtures contain 32 tests:

- `SubprocessTest` uses `posix_spawn`; glibc enters
  `clone(CLONE_VM | CLONE_VFORK)`, which Hermit rejects.
- `DiskInterfaceTest` invokes shell commands while exercising filesystem
  operations and reaches the same path.
- `BuildWithDepsLogTest` executes build commands and reaches the same path.

Run the bounded diagnostic with:

```sh
./experiments/ninja-strict/run.sh --skip-build --probe-full
```

The recorded probe times out at `SubprocessTest.BadCommandStderr` after Hermit
reports unsupported `CLONE_VFORK`. A direct-`fork` Ninja 1.6 control exposed a
second scheduler issue: the parent can block on the child pipe before the new
child receives its initial go-ahead. Consequently, merely replacing
`posix_spawn` does not make the full test suite pass.

The 378-test result is evidence for the parser, graph, build-plan, depfile,
dyndep, log, and other in-process Ninja logic. It deliberately does not claim
child-process determinism. The probe remains in the runner so a future
scheduler fix can convert this limitation into full-suite coverage without
changing the workload.
