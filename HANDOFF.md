# Handoff: `impl-land-ci-cluster-prs` (slot116)

## Status: BLOCKED on a design decision for #644

- Owner: hermit-175 (adopted from hermit-144)
- Slot/CWD: `/home/newton/work/dev-hermit/worktrees/slot116`
- Local branch: `impl-unsupported-syscall-warnings-slot57` @ `093953e0`
- Pushed branch: `impl-644-rebased-onto-main-slot116` @ `093953e0c6b319c459e21c835c7434ff05590b75`

## Cluster state (changed vs the original handoff)

- **#642** — DONE. Closed as redundant; its CI-split work landed via **#673** (merged).
- **#643** — folded into #644 (its classification commit is #644's base).
- **#644** — the only remaining work. Rebased cleanly onto `origin/main` `e5a83fc3`
  as one squashed commit (`093953e0`). Reverie **#84 is MERGED**; all workspace
  crates re-pinned to reverie `2c8aba52d27192bb48c19e50249ea1f11d22cee8`
  (`detcore-liteinst` left at its `main` pin `c28f9c6`).

## Local validation (on `093953e0`, ptrace, release, PMU available)

- `cargo fmt --all -- --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo check --workspace --all-targets` — clean
- `detcore` classification unit tests — 2/2 pass; counts `[112,100,161]`

## THE BLOCKER

#644 makes explicit `--strict` fail closed (`panic_on_unsupported_syscalls`)
and classifies `getppid`/`getpgrp` as `Unsupported`. On `main`, `--strict` did
NOT fail closed (only `FAIL_CLOSED_ENV`), so those syscalls passed through and
`bash` et al. passed L2. Now `bash` (uses `getppid`) fails `--strict`.
**68 of 136** `strict_compatibility_probe` rows in `validate.sh` invoke `bash`,
so they all regress PASS→FAIL. That is the **blocking** strict-compat envelope
(`ci-hosted.yml` → `validate.sh --hosted-only` → `run_strict_compatibility_envelope`;
`validate.sh:2765` fails validation on regression). Hosted CI would go RED.

Not landed, gate not weakened, `getppid` not reclassified (its determinism is a
real judgment — reparenting after the parent exits). Full evidence + 3 options
in the PR comment: https://github.com/rrnewton/hermit/pull/644#issuecomment-5080736202

## Recommended next step

Option 1 (my recommendation): reclassify `getppid`/`getpgrp` as `PassThrough`
(they passed L2 via passthrough on `main`, like `getpid`), and repoint
`tests/c/dbi_unsupported_syscall.c` + `run_dbi_aggregates_...` + the warning
fixtures to a genuinely-unsupported syscall. Then re-rebase, re-run
`./validate.sh --hosted-only`, and land on GitHub-hosted green. Needs author/
human sign-off because it changes #644's fail-closed policy surface.

Do not park/reuse this slot; task remains `in_progress`.
