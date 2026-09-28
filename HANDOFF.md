# LiteInst recovery handoff

## Current checkout

- Worktree: `/home/newton/work/dev-hermit/worktrees/slots/liteinst-session-recovery`
- Branch: `codex/liteinst-session-recovery`
- HEAD/base: `4d8f866102882b6eeabdf99cc7e81433cc3c95c5`
- Index: empty; nothing is staged or committed.
- TaskGraph task: `sud-only-in-guest-mode-with-patching-optional-and-backend-consolidation`
- Task state: `IN_PROGRESS`, owned by the current recovery session (`liteinst-recovery-3`); 646 notes at the last read.
- Do not access or write the original `liteinst-owner` checkout.

## Dirty paths

Tracked modified paths (52):

```text
Cargo.lock
Makefile
README.md
SCORECARD.md
ci/compat-envelope/cells.json
ci/compat-envelope/pressure-test.rs
ci/dag/portable.json
ci/expected-e2e-plan.json
ci/hermetic/run-split-validate.sh
ci/liteinst-strict-node.sh
ci/matrix-symmetry-baseline.json
ci/publish-hermit-e2e-artifact.sh
ci/test-footprints.json
ci/verify-hermit-e2e-artifact.sh
docs/ARCHITECTURE.md
docs/Developers/CargoFeatures.md
docs/SABRE_COMPATIBILITY.md
docs/USER_GUIDE.md
hermit-cli/BUCK
hermit-cli/Cargo.toml
hermit-cli/build.rs
hermit-cli/src/bin/hermit/main.rs
hermit-cli/src/bin/hermit/run.rs
hermit-cli/src/interp.rs
hermit-cli/src/lib.rs
hermit-cli/tests/cli.rs
hermit-cli/tests/common/liteinst.rs
hermit-cli/tests/liteinst_advanced.rs
hermit-cli/tests/liteinst_host_activation.rs
hermit-install/Cargo.toml
hermit-install/build.rs
hermit-install/src/lib.rs
liteinst-runtime-build/Cargo.lock
liteinst-runtime-build/Cargo.toml
liteinst-runtime-build/artifact.rs
liteinst-runtime-build/build.rs
liteinst-runtime-build/src/lib.rs
scripts/check-git-pin-uniformity.rs
scripts/check-nested-lockfiles.rs
scripts/check-reverie-pin.rs
scripts/cross-backend-detlog-diff.rs
scripts/stage-liteinst-runtime.sh
scripts/validate.rs
tests/c/liteinst_host_activation.c
tests/e2e/manifests/applications.yaml
tests/e2e/manifests/backend-parity-c.yaml
tests/e2e/manifests/bin-c.yaml
tests/e2e/manifests/c-programs.yaml
tests/e2e/manifests/debugger-c.yaml
tests/e2e/manifests/inventory/test-files.json
tests/e2e/manifests/language-runtimes.yaml
tests/e2e/manifests/system-utils.yaml
```

Tracked deleted paths (3):

```text
liteinst-runtime-build/runtime/Cargo.toml
liteinst-runtime-build/runtime/src/lib.rs
tests/c/liteinst_inert_runtime.c
```

Untracked status entries before this `HANDOFF.md` was created (12):

```text
hermit-cli/src/liteinst.rs
hermit-cli/src/liteinst/
hermit-cli/src/liteinst_artifact.rs
hermit-cli/src/liteinst_artifact_private.rs
hermit-cli/src/liteinst_bootstrap.rs
hermit-cli/src/liteinst_record.rs
hermit-cli/src/liteinst_record/
hermit-install/liteinst_inputs.rs
liteinst-runtime-build/detcore-runtime/
liteinst-runtime-build/private-native/
liteinst-runtime-build/private_build.rs
tests/c/liteinst_native_child.c
```

`HANDOFF.md` is an additional untracked path. All recovery/build receipts remain below ignored `target/` paths and must not be staged.

## Integrated worker outputs

- `dispatch_fix`: integrated the Hermit dispatch corrections and the isolated Reverie candidate changes and receipts. Generic Reverie `Backend::run*` now refuses with typed `Unsupported`; public ptrace-host, legacy environment, ambient pathname, and unowned launch paths were removed; caller-owned `PreparedCommand` remains the runnable path. Its completion claim is not accepted because nothing is committed, published, pinned, or reviewed at an exact PR head.
- `preload_safety`: integrated the approved sealed-file ownership, exact `LD_PRELOAD` restoration/removal, descriptor cleanup, path-replacement checks, and final staging review. Its earlier unresolved-`liteinst2` concern is superseded: `rrnewton/liteinst2` revision `95ee5e6917fa33191eb41c3f1606ea8b03c1b78c` is publicly fetchable and crates.io has `liteinst2 0.1.0`.
- `signal_safety`: integrated the signal-mask review and all valid backend-review findings. Attach/retire signal handling passed source review without scheduler or virtual-time changes. Its findings about legacy launch paths, native-test wiring, evidence, and assurance wording drove the later Reverie corrections. Backend evidence remains B1 at most. The `Tool` API changes trigger post-facto human-review criterion 2.
- `staging_build_wiring`: integrated and independently approved source-record/build ordering, cache identity, artifact/provenance pair validation, native wrapper/link checks, install checks, generated manifests, and ordinary-CLI refusal. All 1,077 LiteInst E2E cells are not applicable; zero are Green or measured, and the ordinary CLI plan selects zero LiteInst cells.

## Hermit checks and receipts

Completed checks include:

- `cargo test --offline --locked --manifest-path liteinst-runtime-build/Cargo.toml`: 31 passed.
- `cargo test --offline --locked -p hermit-install liteinst_inputs`: 3 passed.
- `ci/liteinst-strict-node.sh --self-test`: 12/12 passed.
- `scripts/cross-backend-detlog-diff.rs` self-test: 31/31 passed.
- Nested workspace metadata, lock checks, inventory checks, CI audit, generated expected plan, footprint checks, formatting, shell syntax, JSON parsing, and `git diff --check` passed at their recorded revisions.
- Normal Hermit compilation remains refused because the checked-in Reverie pin does not contain the recovered package and APIs.

Relevant generated-state result:

- 5,744 total scorecard cells: 649 Green, 157 red, 4,938 not applicable.
- LiteInst: 1,077 not applicable, zero Green, zero red, zero measured, zero observations.

## Current Reverie candidate

- Candidate: `/home/newton/work/dev-hermit/worktrees/slots/liteinst-session-recovery/target/liteinst-recovery-3/reverie-publication-prep`
- Candidate Git state: detached at base `320412c5967790939ebe405c73e394ffd9c41459` with an uncommitted recovered diff.
- Current source-manifest seal: `87e6c3a7483755914cd76c80190aa57faa66aa2361d61e7b45acf1ed0106ee74`
- Current tracked `git diff --binary HEAD` SHA-256: `cb192cdaaac07682b29acc772156173f7a37f0d3835aa7e3ca1dca3cee0c7c23`
- Lifecycle receipt: `target/liteinst-recovery-3/reverie-publication-audit/final-lifecycle.kwMIyC/`
- Final native receipt: `target/liteinst-recovery-3/reverie-publication-audit/frozen-native.cTmi87/`
- The two lifecycle corrections are complete, not partial. Their focused tests pass 1/1 each and the full serial LiteInst library suite passes 69/69.
- The self-contained owned-public integration target previously passed 17/17 with no ignored tests; the exact native case below was rerun after the final freeze.

## Required unsandboxed native integration result

Candidate cwd:

```text
/home/newton/work/dev-hermit/worktrees/slots/liteinst-session-recovery/target/liteinst-recovery-3/reverie-publication-prep
```

Material environment:

```text
OWNED_PUBLIC_INSTRUCTION_ARTIFACTS=/home/newton/work/dev-hermit/worktrees/slots/liteinst-session-recovery/target/liteinst-recovery-3/reverie-publication-audit/frozen-native.cTmi87/artifacts
```

Exact unsandboxed command:

```text
timeout 180s cargo test -p reverie-liteinst-runtime --test owned_public_instruction public_owned_native_routes_cpuid_and_rdtsc_to_the_shared_tool -- --exact --test-threads=1 --nocapture
```

Result:

- Terminal exit code: 0.
- Wall time: 3.135714857 seconds (reported as 3.136 seconds).
- Libtest time: 0.05 seconds.
- Executed: 1.
- Passed: 1.
- Failed: 0.
- Ignored: 0.
- Filtered: 16.
- First failure: none.
- Retained outcome: `ok`, exit status 0, expected six events and three injections, empty guest stderr.

The unsandboxed native test passed 1/1 in 3.136 seconds. This is a Reverie native integration result only. No Hermit guest/backend result exists.

## Remaining blockers

1. Nothing is staged, committed, pushed, or in a pull request. Neither `implemented` nor `landed` is supported.
2. Hermit and the nested runtime still pin Reverie `320412c5967790939ebe405c73e394ffd9c41459`; that revision lacks `reverie-liteinst-runtime` and the recovered APIs.
3. The isolated Reverie candidate must receive a final independent review, then be committed and published before Hermit pins and lockfiles can be updated.
4. The Reverie core `Tool` API changes require post-facto human-review criterion 2, including exact-head dual review after a pull request exists.
5. No Hermit guest/backend command has executed successfully. Generic public LiteInst backend entry points intentionally refuse until caller-owned Session integration is used.
6. An over-broad runtime `private-crt --all-targets` Clippy attempt remains red on 94 pre-existing test-style lints. The intended strict runtime `--lib --features private-crt` and focused package checks are Green; do not relabel the over-broad result.
7. Required product source is still untracked, and generated files are dirty. Preserve explicit-path staging discipline; never use `git add -A`.

## Relevant worker sessions

- `/root/dispatch_fix`: completed the candidate changes, lifecycle fixes, native execution, receipts, and this read-only inventory.
- `/root/dispatch_fix/final_blockers`: completed; its older receipt precedes the final lifecycle changes.
- `/root/dispatch_fix/lifecycle_fix`: interrupted only after its edits and Green receipts were complete; it left no partial edit.
- `/root/preload_safety`: completed review and verification work.
- `/root/signal_safety`: completed signal and backend reviews; its last review predates the final ambient-route/native-gate/lifecycle corrections.
- `/root/staging_build_wiring`: completed implementation and review corrections.

Do not restart completed workers or rerun finished checks without a concrete failure at the current source identity.

## Single next safe action

Perform one read-only independent review of the exact current Reverie candidate identified by source seal `87e6c3a7483755914cd76c80190aa57faa66aa2361d61e7b45acf1ed0106ee74` and diff SHA-256 `cb192cdaaac07682b29acc772156173f7a37f0d3835aa7e3ca1dca3cee0c7c23`, including the final ambient-route, native-validation, and lifecycle corrections. Do not commit, publish, update Hermit pins, or claim backend success before that review reports no blocking finding.
