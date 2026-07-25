# e9patch record/replay coverage

Measured: 2026-07-25

## Result

PR #696 adds the missing e9patch record CLI path and fixes replay-output
truncation under pipe backpressure.

The post-fix `validate.sh`-derived matrix passes completely:

- Ptrace: **20/20 PASS R/R**.
- E9patch: **20/20 PASS R/R**.
- E9patch exercised four rewritten executables: `gcc` (28 sites), `g++`
  (28), `cpp` (28), and `gcov` (10).
- The other 16 e9patch rows exercised the zero-site path.
- Every passing row recorded and replayed with exit status 0 and
  byte-identical stdout.

Record mode is strict by definition. These are record/replay compatibility
results, not L1-L4 `run --verify` assurance claims.

## Snapshot

- Base commit: `f0b9eff2a864cd1e683cbf56fe2af55fba28d9f2`
  (`Expand strict scripting language coverage (#692)`).
- Branch: `impl-e9patch-rr-cli-integration-slot115`.
- Pull request: #696, `Add e9patch record and replay integration`.
- Host: x86_64 Linux `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79`.
- CPU: AMD EPYC 9D85 158-Core Processor.
- Optimized post-fix Hermit SHA-256:
  `1aabb311d03be200a95c7d77f857464284287405000f6f6396dd9257fe2f916d`.
- e9tool SHA-256:
  `8569c9c62f2b9ad79f22903ae01b58d99abad438023f7a4d49538785419625d0`.
- e9patch SHA-256:
  `083e7deee709d66b82ca9e3692c7cd31326e64fdcec515704c769d336320d5fe`.
- Matrix build: post-fix debug build; final rewritten-GCC smoke also passed
  with the optimized build above.
- Log level: default.
- Relaxations: none.

## Integration

`hermit --backend e9patch record` now:

1. Resolves the guest executable and runs the cached e9patch preprocessor.
2. Bind-mounts the prepared ELF read-only over the canonical original path in
   the recording container.
3. Records the original program, argv, and executable path while the existing
   recorder copies the prepared bytes into `recording/exe`.
4. Replays from that saved executable through the existing ptrace replayer.

Replay intentionally remains backend-independent. It does not need e9tool,
e9patch, or the instruction-map cache after recording.

The shared replay output path now waits for `POLLOUT` and retries after
`EAGAIN` instead of silently dropping the unwritten suffix. The regression
test saturates a pipe with a 256 KiB payload and verifies every byte reaches a
delayed reader.

## Method

Each matrix row used a fresh recording home and bounded phases:

```text
ptrace record:
  timeout 90s hermit --backend ptrace record start \
    --data-dir CASE/recording-home --record-timeout 75 -- PROGRAM ARGS...

e9patch record:
  HERMIT_E9TOOL=... HERMIT_E9PATCH_BACKEND=... \
  timeout 90s hermit --backend e9patch record start \
    --data-dir CASE/recording-home --record-timeout 75 -- PROGRAM ARGS...

replay, with no e9patch environment:
  timeout 90s hermit replay --autopilot \
    --data-dir CASE/recording-home
```

Success required record exit 0, replay exit 0, and byte-identical stdout.

## Comparison table

| Program | Workload | Ptrace R/R | E9patch R/R | Mapped sites |
| --- | --- | --- | --- | ---: |
| `echo` | `echo hermit-compat` | PASS | PASS | 0 |
| `true` | no arguments | PASS | PASS | 0 |
| `pwd` | no arguments | PASS | PASS | 0 |
| `seq` | `seq 10` | PASS | PASS | 0 |
| `cat` | `cat README.md` | PASS | PASS | 0 |
| `wc` | `wc -c README.md` | PASS | PASS | 0 |
| `head` | `head -n 3 README.md` | PASS | PASS | 0 |
| `base64` | encode `README.md` | PASS | PASS | 0 |
| `base32` | encode `README.md` | PASS | PASS | 0 |
| `id` | `id -u` | PASS | PASS | 0 |
| `lua` | `print(42)` | PASS | PASS | 0 |
| `perl` | print `42` and newline | PASS | PASS | 0 |
| `awk` | `BEGIN { print 42 }` | PASS | PASS | 0 |
| `sqlite3` | in-memory insert/count/sum | PASS | PASS | 0 |
| `bash` | deterministic three-line loop | PASS | PASS | 0 |
| `gcc` | `--version` | PASS | PASS | 28 |
| `g++` | `--version` | PASS | PASS | 28 |
| `make` | `--version` | PASS | PASS | 0 |
| `cpp` | `--version` | PASS | PASS | 28 |
| `gcov` | `--version` | PASS | PASS | 10 |

## Focused evidence

The rewritten GCC recording proves identity preservation and cache-independent
replay:

- E9patch diagnostic: 28 candidate sites, 28 mapped sites, 0 B0 sites.
- Recording metadata `exe`, `program`, and `arg0`: `/usr/bin/gcc`.
- Prepared artifact and saved `recording/exe` SHA-256:
  `73849134719da2234953fe22a2b5f97ac6fa9aa985cabb7e0a18135b953b8dae`.
- Record/replay stdout: byte-identical.
- Replay passed without `HERMIT_E9TOOL` or `HERMIT_E9PATCH_BACKEND`.
- `hermit --backend e9patch record start --verify -- /usr/bin/gcc --version`
  also reported `Success: replay matched recording` in debug and optimized
  builds.

The former `cat` regression is fixed at default logging:

- Source, record stdout, and replay stdout: 13,331 bytes each.
- Record/replay stdout SHA-256:
  `8433d783d9d1c305f3b7c0d0b88dec6ab763b4a3e80fe24f386db39b6f6fcbc0`.

## Remaining failure

An executable Python shebang script correctly takes the e9patch non-ELF
fallback (`mapped_sites=0; preprocessing=not-applicable`) and records
successfully, but replay fails while constructing its chroot:

```text
Error: Failed to create chroot environment
     > File exists (os error 17)
```

The identical ptrace record/replay case fails the same way, so this is an
existing shebang replay limitation rather than an e9patch regression. The
20-program matrix contains ELF entrypoints and is unaffected.

## Validation

- `cargo test -p hermit --lib --bin hermit`: 51 library and 62 CLI tests
  pass.
- `cargo test -p hermit --test record_replay -- --test-threads=1`: all 33
  tests pass. A parallel full-package run exceeded one timeout test's
  15-second wall-clock bound under concurrent load; its isolated rerun passed
  in 1.03 seconds.
- `cargo fmt --all -- --check`: pass.
- `cargo clippy -p hermit --all-targets -- -D warnings`: pass.
- Ptrace/e9patch matrix: 20/20 and 20/20 PASS R/R.
- Optimized rewritten-GCC `record --verify`: pass.

Raw local evidence:

- `/tmp/e9rr-results-pr696.csv`
- `/tmp/e9rr-matrix-pr696.GZWj3S/`
- `/tmp/e9rr-pr696-gcc.0kGqb8/`
- `/tmp/e9rr-pr696-cat.3HqqNW/`
- `/tmp/e9rr-pr696-release.Gkh6A4/`
