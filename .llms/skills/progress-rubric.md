# Hermit Progress Rubric

Use this skill when asked for a Hermit progress report, frontier comparison, backend parity
snapshot, or evidence-based coverage matrix. The checked-in report is generated evidence; do not
edit its totals by hand.

## Deliverables

The rubric has three parts:

1. This file defines stable categories, columns, counting, and regeneration steps.
2. `scripts/progress-report.sh` runs the cases and generates structured evidence.
3. `docs/PROGRESS_REPORT.md` and `docs/PROGRESS_REPORT.tsv` are generated outputs.

## Matrix Contract

Generate four matrices for both `rrnewton/hermit` `main` and the current `frontier` (or
`speculative` when no frontier exists):

1. `hermit run` strict determinism using ptrace.
2. The same strict probes using `--backend dbi`.
3. The same strict probes using `--backend kvm`.
4. Record/replay.

Every matrix uses these rows:

- Basic system binaries
- rr test suite
- OSS full apps
- Unit tests
- Integration tests

Every matrix uses these language columns:

- C/C++
- Rust
- Python
- Java
- Go
- Ruby
- OCaml
- Node.js
- Other

C and C++ intentionally share one column. At Hermit's boundary their syscall behavior is the
useful comparison, not their source-language distinction.

After the matrices, report actual chaos, debugger-attachment, and schedule-bisection cases.

## Counting Rules

- A cell is `passed/attempted`.
- A named language probe is one case. C/C++ has separate C and C++ probes, so it can contribute two
  cases.
- A named Rust/libtest test is one case. The runner parses the test process output rather than
  treating the whole Cargo invocation as a single pass.
- `SKIP` is evidence, but not an attempt. Use it only for an absent target, ignored test, missing
  explicit dependency, or unsupported setup.
- A timeout, hardware limitation, backend initialization error, missing expected marker, or test
  assertion is `FAIL`, not `SKIP`.
- Never infer a pass from source inspection, an old result file, or CI. CI status is context only.
- Strict basic-binary probes require two successful paths: an independent marker run proving the
  requested program executed, and Hermit's built-in normalized `--verify` run proving determinism.
  Do not compare raw tool stderr because timestamped diagnostics are not guest output.
- Backend probes validate the requested program's marker. This matters for the current KVM
  prototype, which may exit zero after running a built-in guest instead of the requested ELF.
- `-` in Markdown means that no runnable case contributed to the cell. Consult the TSV for skips.

## Preparation

Use clean, isolated worktrees. Do not switch the user's active checkout.

```bash
git fetch origin
mkdir -p ~/work/dev-hermit/worktrees
git worktree add --detach ~/work/dev-hermit/worktrees/progress-main origin/main
git worktree add --detach ~/work/dev-hermit/worktrees/progress-frontier origin/frontier
```

Do not place executable test worktrees under host `/tmp`. Hermit virtualizes the guest `/tmp`, and
current frontier builds reject guest programs whose host path is below `/tmp`.

If `origin/frontier` does not exist, use `origin/speculative` and state that choice in the report.
Initialize rr only when the rr suite should be attempted:

```bash
with-proxy git -C ~/work/dev-hermit/worktrees/progress-frontier \
  submodule update --init third-party/rr
```

The supported host is x86_64 Linux with the repository's nightly toolchain and libunwind headers.
Install the language runtimes named by the matrix. Missing runtimes are recorded as skipped cases.
For CI metadata, export the required GitHub proxy:

```bash
export HTTPS_PROXY=http://fwdproxy:8080
```

## Generate

Run the script from the worktree where the generated files should be written:

```bash
scripts/progress-report.sh \
  --main-worktree ~/work/dev-hermit/worktrees/progress-main \
  --frontier-worktree ~/work/dev-hermit/worktrees/progress-frontier \
  --output docs/PROGRESS_REPORT.md \
  --data docs/PROGRESS_REPORT.tsv \
  --artifacts /tmp/hermit-progress-artifacts
```

Use `--skip-build` only after both worktrees have successfully completed `cargo build --workspace`.
Use `--case-timeout` to change the per-probe limit; suite-specific limits remain intentionally
larger. Preserve the artifact directory until failures have been reviewed.

## Review Checklist

1. Confirm both SHAs in the report match the requested refs.
2. Confirm the TSV contains only results from the current run.
3. Inspect every `FAIL` log; do not relabel environmental failures as passes.
4. Check that KVM cases include marker validation and do not count the built-in hello guest.
5. Check that ignored and missing-dependency cases appear under skipped coverage.
6. Confirm `bash -n scripts/progress-report.sh` and `git diff --check` pass.
7. State whether frontier has a clean CI run; absence of CI is not a test pass or failure.

## Extending The Rubric

Add a probe when a new language, backend, or major product category becomes supported. Keep case
names stable so reports are comparable over time. Add actual commands to the runner first, then
regenerate both output files. Do not manually add a matrix number without a corresponding TSV row
and command log.
