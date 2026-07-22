# AGENTS.md

This file applies to the entire repository.

## Workspace Discipline

All mutating agent work must happen in an assigned private worktree. Never let
two agents modify the same checkout, and never do feature development in the
primary checkout.

### Vocabulary And Layout

- **Parent**: `~/work/dev-hermit/`, the local multi-agent development root.
- **Primary checkouts**: coordinator-owned integration surfaces that ordinary
  agents read but never edit. There are two per product:
  - `~/work/dev-hermit/hermit/` on `frontier` and
    `~/work/dev-hermit/main/hermit/` on `main` for Hermit.
  - `~/work/dev-hermit/reverie/` on `frontier` and
    `~/work/dev-hermit/main/reverie/` on `main` for Reverie.
  The primaries donate their warm `target/` caches to new slots.
- **Slot**: a numbered feature worktree, `slotNN`. Each slot is a **direct**
  Git worktree — the checkout root itself, not a subdirectory. Hermit slots
  live at `~/work/dev-hermit/worktrees/slotNN`; Reverie slots live at the
  parallel path `~/work/dev-hermit/worktrees_reverie/slotNN`.
- **Feature branch**: a descriptive, task-specific branch checked out in one
  slot. Slot directory names stay opaque and stable even as branches change.
- **Primary branch**: `main` in `rrnewton/hermit`, the continuously tested
  development branch checked out in `main/hermit/`.
- **Upstream**: `facebookexperimental/hermit`, which receives periodic bulk
  pull requests from tested fork `main` rather than day-to-day feature work.

Expected layout:

```text
~/work/dev-hermit/
|-- hermit/                     # Hermit primary; frontier; coordinator only
|-- reverie/                    # Reverie primary; frontier; coordinator only
|-- main/
|   |-- hermit/                 # Hermit primary; main; rebase base
|   `-- reverie/                # Reverie primary; main; rebase base
|-- ACTIVE.md                   # Hermit worktree assignments (source of truth)
|-- ACTIVE_REVERIE.md           # Reverie worktree assignments
|-- ARCHIVED.md                 # Hermit completed-slot history (append-only)
|-- ARCHIVED_REVERIE.md         # Reverie completed-slot history (append-only)
|-- worktrees/
|   |-- slot01                  # direct Hermit worktree (feature branch)
|   |-- slot02
|   `-- slotNN
`-- worktrees_reverie/
    |-- slot01                  # direct Reverie worktree (feature branch)
    |-- slot02
    `-- slotNN
```

Hermit and Reverie worktrees are independent: a Hermit-only task uses a
`worktrees/slotNN` checkout and leaves any matching `worktrees_reverie/slotNN`
untouched. A coordinated change uses the same slot number in both trees. Do not
use branch names as worktree directory names. Do not create ad hoc checkouts
elsewhere for normal work. If a requested slot contains unexpected changes,
treat it as occupied: do not reset, clean, overwrite, or reuse it. Select
another free slot and report the conflict to the coordinator.

### Slot Pool

Reuse existing numbered slots instead of removing and recreating worktrees.
Keeping the worktree and its ignored `target/` directory avoids repeated
dependency downloads and full rebuilds.

A slot is in one of two states:

- **Active**: checked out on a feature branch, owned by one agent/task, and
  listed in the relevant `ACTIVE*.md`.
- **Parked**: clean, detached at a recorded commit, absent from `ACTIVE*.md`,
  and available for reuse.

Parking happens in place; do not `git worktree move` a slot. A detached slot is
not abandoned work: its completed feature branch and commit SHA must already be
recorded in the handoff and in `ARCHIVED*.md`.

Creating slots is a coordinator operation. From the parent root, create a
Hermit and/or Reverie slot from the primaries with the helper:

```bash
./slot-init.sh slot0X            # Hermit + Reverie worktrees for slot0X
./slot-init.sh slot0X hermit     # Hermit worktree only
./slot-init.sh slot0X reverie    # Reverie worktree only
```

The helper runs `git -C <primary> worktree add` against the owning repository
(`hermit` for `worktrees/slotNN`, `reverie` for `worktrees_reverie/slotNN`),
never `git worktree add` from the parent. The primary is the build-cache donor.
When a new or refreshed slot has no `target/`, seed it with a copy-on-write
copy when the filesystem supports reflinks:

```bash
cp -a --reflink=auto ~/work/dev-hermit/hermit/target \
  ~/work/dev-hermit/worktrees/slot0X/
```

Never symlink `target/` between checkouts: concurrent Cargo processes must not
write to the same target directory. Cache seeding is optional when the donor
does not exist or is stale; correctness must not depend on cached artifacts.

### Starting Work In A Slot

The coordinator assigns one free slot to exactly one agent. Before editing:

1. Confirm the slot is a registered worktree and inspect its state:

   ```bash
   git -C ~/work/dev-hermit/hermit worktree list
   git -C ~/work/dev-hermit/worktrees/slot0X status --short --branch
   ```

2. Require a clean worktree. Do not discard or absorb changes left by another
   task.
3. Fetch fork refs, then create a descriptive branch from current fork `main`:

   ```bash
   git -C ~/work/dev-hermit/worktrees/slot0X fetch origin
   git -C ~/work/dev-hermit/worktrees/slot0X switch \
     -c <feature-branch> origin/main
   ```

4. Record the slot, branch, task, and owner in the relevant `ACTIVE*.md` and in
   the coordinator's task state before the first edit.

Agents may read the primary checkouts, including their build artifacts, but
they must run edits, formatting, builds, tests, and commits from their assigned
slot.

### Parking And Reusing A Slot

Park a slot only after all intended work is committed and handed off:

```bash
git -C ~/work/dev-hermit/worktrees/slot0X status --short
git -C ~/work/dev-hermit/worktrees/slot0X switch --detach HEAD
```

The first command must produce no output. Record the feature branch name, exact
HEAD SHA, validation performed, and landing status in `ARCHIVED*.md` and remove
the slot's row from `ACTIVE*.md` before marking it free. Do not delete a feature
branch until its commit is reachable from fork `main` or the coordinator
explicitly archives it.

To reuse a parked slot, re-run the starting-work checks and create the next
feature branch from the latest `origin/main`. A slot that is not clean remains
active regardless of whether an agent is currently responding.

### Branch And Integration Strategy

The branch flow is:

```text
feature branches -> rrnewton/hermit main -> periodic upstream pull request
```

- Agents branch from `origin/main` in the `rrnewton/hermit` fork and never
  develop directly on `main`.
- Each feature branch contains one coherent task or tightly coupled change.
- The agent validates and commits the feature branch, pushes it to `origin`,
  and opens a pull request against fork `main`.
- The landing coordinator reviews the diff, local test evidence, and fork CI
  before merging it.
- Direct emergency landing onto `main`, when explicitly authorized, uses a
  fast-forward merge from the clean primary checkout:

  ```bash
  git status --short --branch
  git merge --ff-only <feature-branch>
  ```

- If the fast-forward check fails, do not create a convenience merge commit.
  Return the branch to its owner to update it against current `origin/main`,
  rerun affected tests, and provide a new SHA.
- Keep fork `main` green; repair a regression before accepting more feature
  work.
- Periodically submit a reviewed, green batch from fork `main` to
  `facebookexperimental/hermit` as one upstream pull request. Do not use the
  upstream repository for routine feature branches or CI iteration.

Only the coordinator mutates the primary checkout. It must be clean before a
merge or promotion. Unrelated primary-checkout changes are a blocker to that
landing operation and must be attributed and resolved without destructive
cleanup.

### Commit Methodology

Agents prepare clean, reviewable commits rather than leaving uncommitted files
for the coordinator:

- Inspect `git status` and the complete diff before staging.
- Stage only task-owned paths. Keep generated artifacts, caches, debug output,
  and unrelated concurrent changes out of commits.
- Run focused tests while iterating, then the formatting, lint, and test gates
  required by the change before handoff.
- Prefer one logical commit per task. Split commits only when each commit is
  independently coherent and useful to reviewers.
- Write an imperative, descriptive subject that states what changed. Explain
  the reason and non-obvious constraints in the body when needed.
- Never use placeholder subjects such as `wip`, `tmp`, `checkpoint`,
  `validate`, or `fix stuff`.
- Do not create empty bookkeeping commits. Do not hide test failures or missing
  validation in a commit message; report them explicitly in the handoff.
- Rewrite or amend only commits that are still private to the agent's own
  feature branch. Never rewrite a shared branch or a commit already integrated.
- Do not push, force-push, merge, rebase, or delete branches unless the task or
  coordinator explicitly authorizes that repository-side operation.

Every handoff includes:

- slot path and feature branch,
- exact commit SHA and concise change summary,
- commands run and their results,
- known failures or environment limitations,
- whether the branch is ready for a pull request or authorized fast-forward.

## Project Overview

Hermit is an x86_64 Linux reproducible container. It runs a guest program under
the Reverie instrumentation layer, intercepts syscalls and other events, and
uses Detcore to replace or sanitize sources of nondeterminism such as time,
randomness, and thread scheduling. The project is in maintenance mode, so keep
changes focused, preserve existing behavior, and add regression coverage when
fixing bugs.

Hermit does not make a changing filesystem or external network deterministic.
Tests that need full isolation must provide a stable filesystem and avoid
external network dependencies.

## Supported Environment

- Use x86_64 Linux. AArch64 support is incomplete, and macOS is not supported.
- The repository's `rust-toolchain.toml` selects Rust nightly. Let `rustup` and
  Cargo honor that file; do not substitute stable Rust without a task that is
  specifically about stable support.
- Install libunwind development headers before building. On Debian/Ubuntu use
  `sudo apt-get install -y libunwind-dev`; on Fedora/CentOS use
  `sudo dnf install -y libunwind-devel`.
- Deterministic preemption uses the CPU Performance Monitoring Unit to count
  retired conditional branches (RCBs). Some Hermit runs require accessible
  hardware performance counters and may not work in restricted containers or
  virtual machines.
- CPUID behavior also varies across hosts. In particular, the Detcore
  `tests_misc` RDRAND/RDSEED tests can fail when a VM exposes unusual CPU
  features or prevents CPUID interception. Report the host limitation rather
  than weakening the assertion without evidence of a product bug.

## Build And Run

Run commands from the repository root.

```bash
cargo build --workspace
```

The main binary is `target/debug/hermit`. A basic invocation is:

```bash
./target/debug/hermit run -- <program> [args...]
```

Use `cargo check --workspace --all-targets` for a faster compile-only check.
The Cargo manifests are generated by `autocargo` from Meta's internal Buck
targets, as noted in each manifest header. Do not casually hand-edit generated
manifests or remove `@fb-only`/`@oss-only` export markers. If a task requires a
manifest change, keep it minimal and explain how the generated source should
remain synchronized.

## Test

Run the public Cargo test suite with:

```bash
cargo test --workspace
```

During iteration, prefer the narrowest relevant target, for example:

```bash
cargo test -p detcore
cargo test -p detcore --test tests_time
cargo test -p hermit
```

The `tests` and `flaky-tests` workspace members mostly contain guest binaries
that Hermit executes in different modes; they are not themselves the complete
end-to-end test matrix. Meta's internal Buck setup has more than 700
integration tests combining those guests with run modes and the rr suite. That
matrix has not been fully ported to the public Cargo build. Do not interpret a
green `cargo test --workspace` as coverage of every internal integration test.

When a test depends on PMU access, CPUID interception, or particular CPU
features, include the environment in failure reports. Do not mark or delete a
hardware-sensitive test merely to make a local VM green.

## Lint And Format

The checked-in rustfmt configuration requires the selected nightly toolchain.
Before finishing a Rust change, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Use `cargo fmt --all` to apply formatting. Fix warnings in code you change; do
not add broad `allow` attributes unless the warning is intentionally inapplicable
and the reason is documented.

## Workspace Map

The root Cargo workspace has ten members:

| Path | Role |
| --- | --- |
| `hermit-cli` | Main `hermit` CLI and `libhermit`; run, record, replay, log-diff, analyze, and container orchestration. |
| `detcore` | Core determinism engine and Reverie tool; scheduler, virtual time, syscall handling, CPUID handling, and record/replay behavior. |
| `detcore-model` | Shared deterministic state and model types, including PIDs, file descriptors, futexes, schedules, and logical time. |
| `detcore/tests/testutils` | Helpers used by Detcore integration tests. |
| `hermit-verify` | Verification executable for stress, trace, schedule, and replay checks. |
| `common/digest` | Digest utility shared by the workspace. |
| `common/edit-distance` | Edit-distance utility used when comparing executions and logs. |
| `common/test-allocator` | Test allocator and supporting test binary. |
| `tests` | Guest programs used by integration scenarios, including time, futex, network, pipe, scheduling, and RDTSC cases. |
| `flaky-tests` | Intentionally racy guest programs used to demonstrate and test deterministic scheduling and chaos mode. |

## Architecture

Reverie is the external process-instrumentation layer. It traps guest syscalls,
signals, CPUID/RDTSC operations, and timer-preemption events and can inject
syscalls into Linux. `hermit-cli` creates the container and instantiates Reverie
with Detcore as its tool. Detcore then either emulates an operation or forwards
it to Linux and sanitizes the result.

Detcore serializes guest threads so they effectively share one CPU, chooses the
next runnable thread deterministically, and uses PMU RCB counts for repeatable
preemption. Its implementation is split between `tool_local`, which handles
events near each guest task, and `tool_global`, which owns shared deterministic
state; the two communicate through RPC.

Start investigations in these locations:

- `detcore/src/syscalls/` for syscall-specific behavior.
- `detcore/src/scheduler.rs` and `detcore/src/scheduler/` for scheduling.
- `detcore/src/tool_local.rs` and `detcore/src/tool_global.rs` for event flow
  and shared state.
- `detcore/src/cpuid.rs`, `detcore/src/time.rs`, and
  `detcore/src/record_or_replay.rs` for those subsystems.
- `hermit-cli/src/bin/hermit/` for CLI subcommands.
- `hermit-cli/src/recorder/` and `hermit-cli/src/replayer/` for log recording
  and replay.
- `docs/Developers/Architecture.md` for the architecture overview and
  `docs/Users.md` for user-facing behavior.

## Change Guidelines

- First reproduce a bug with the smallest applicable test or guest program.
- Keep syscall fixes local to the relevant Detcore subsystem when possible.
- Preserve determinism: avoid host wall time, uncontrolled randomness,
  iteration-order dependencies, and host-specific state in guest-visible
  results.
- Add a regression test at the lowest useful layer. A guest program belongs in
  `tests` or `flaky-tests`; Detcore unit and integration behavior belongs under
  `detcore`.
- Run the focused test while iterating, then the workspace test, format, and
  Clippy checks before handing off. Clearly document checks that the current
  hardware cannot execute.
- Keep unrelated changes and generated artifacts out of the patch. Do not
  overwrite concurrent work in a dirty checkout.

## Repository And Issues

Primary development happens in the `rrnewton/hermit` fork. Configure `origin`
for that fork and `upstream` for `facebookexperimental/hermit`. Meta's internal
source uses Buck and is exported upstream; this team develops and runs CI on
fork `main`, then periodically submits a tested bulk pull request upstream.
Follow `CONTRIBUTING.md`, update documentation for user-visible changes, and
never publish security vulnerabilities as ordinary issues.

GitHub Issues are the public issue tracker. On Meta devservers, direct GitHub
API access is unavailable, so set the proxy for every `gh` invocation:

```bash
export HTTPS_PROXY=http://fwdproxy:8080
gh issue list -R rrnewton/hermit
gh issue view <number> -R rrnewton/hermit
```

The proxy is an environment requirement, not an authentication workaround. If
`gh auth status` fails without it, retry with `HTTPS_PROXY` before concluding
that the token is invalid. Create, edit, or close issues only when the task
explicitly calls for that repository-side change.
