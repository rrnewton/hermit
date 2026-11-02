Owner: musetrain (muse, devbig030) since 2026-11-06T01:05Z

Shepherd update: cherry-picked the approved ADRP+LDR reader-gap fix `40cf47bb3092269dd1e29478d51a6b9aad82a0ff` (REWRITE_FORMAT_VERSION 6->7) onto `drain/4735`.

- Previous head: `049ec30043500f567345b3534641e2f006598f94`
- New head: `90bbe4baf480b5d1edbeb9584ee3a77abb1618ab` (clean cherry-pick, no conflicts; author preserved, committer hermit-bot3)
- Recovery: `refs/tbh/recovery/before-discard/20261106T005617Z-1255677` saved in clean worktree `temp/drain-4735` before the pick
- Push: lease-push `049ec300435..90bbe4baf48 -> drain/4735`, ls-remote read-back matches

Targeted evidence at exact new head (x86_64 host; unit/planner scope, no aarch64 execution claimed):
- `cargo test -p hermit-detcore --lib aarch64_lse`: 77 passed (includes `adrp_readers_protect_the_bytes_they_read` at detcore/src/aarch64_lse/elf.rs)
- `cargo test -p hermit-detcore --lib record_serde/reserved_input/preemptions`: 151 passed
- `cargo test -p hermit-run --lib` provenance 5-selection: 5 passed
- `cargo test -p hermit-run --lib aarch64_lse`: 1 passed
- `cargo fmt --all -- --check`: clean; `cargo clippy -p hermit-detcore -p hermit-run --all-targets -D warnings`: clean
- `generate-validation-dag --check`: canonical and fresh. DAG delta is pins 3581->3586 (+5 units) only; onto fresh main (1db0a95883b) merge-tree conflicts remain only the mechanical `ci/dag/validate.json` + `validation_dag_static.rs`, resolved by queue regen (pins = main + delta, never --allow-drop)
- `cargo test -p hermit-manifest-plan`: 707 passed, 7 failed (runner backend-parity probes; unrelated files, host-dependent; not DAG logic)

Review routing: base `47a7f9abe2e528902e82ee41b573763d036f3497` approved by mushep18 (muse); delta through `049ec300` approved by revastra2 (codex, provenance-r4). The 4 post-approval commits plus this fix need delta review at the new head. Not landed; coordinator submits to the merge queue.
