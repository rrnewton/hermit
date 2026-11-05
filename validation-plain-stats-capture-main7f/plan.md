Statistics capture rerun plan (not execution evidence)

Fixed bases: Hermit7f53641d93b2332b1ce53f1b97026cd0d32cb9f1 and Reverie16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d. Baseline owns new clean successors at `hermit-stats-capture-main7f` and `reverie-stats-capture-main16ff`; actual commit IDs will be recorded before execution. Old H342daf/Rff30 records remain untouched. Reviewer confirms the narrowed API/test scope; execution waits for the clean pair release/source join.

Use each repository's own nightly and existing shared Cargo home/working target. Commands run sequentially under `nice -n 19 taskset --cpu-list 0-39`, with `CARGO_BUILD_JOBS=40` and `CMAKE_BUILD_PARALLEL_LEVEL=40`. R alone uses the ordinary short socket TMPDIR at slot `ignored/d1tmp`; H unsets TMPDIR. No stack, timeout, test assertion, or selection relaxation. No composer/dagrun/interval/fullvalidate/network. Current Cargo home is `ignored/dbt-pty-main885-v1/cargo-home`; target is `ignored/dbt-baseline/fork-tree-278-vs452/runtime-capture-substrate/owned-publication-run/target-hermit`. Both were preserved during cleanup.

Before the first build, retain a fresh `df -B1 /data` sample and conservative 64 GiB additional-footprint estimate, plus projected free bytes. Do not build below 650,000,000,000 bytes free. Last cleanup close observed 784,896,380,928 bytes, but this is not a substitute for the build-time sample. Retain closing/boundary free-space observations; do not infer attributable incremental use or peaks from host df deltas. No new monitoring harness.

R first, default Cargo test profile, --offline --locked:

- `cargo metadata --offline --locked --format-version 1`
- `cargo test --offline --locked -p reverie-dbt --lib launcher::tests::stats_capture -- --test-threads=1` (all six new capture tests)
- `cargo test --offline --locked -p reverie-core --lib -- --test-threads=1`
- `cargo test --offline --locked -p reverie-dbt --lib -- --test-threads=1`
- `cargo test --offline --locked -p reverie-dbt --no-default-features --lib -- --test-threads=1`
- `cargo clippy --offline --locked -p reverie-core -p reverie-dbt --all-targets -- -D warnings`
- `RUSTDOCFLAGS='-D warnings' cargo doc --offline --locked -p reverie-core -p reverie-dbt --no-deps`
- `cargo fmt --all -- --check`

The six capture tests preserve missing vs empty, unreadable vs malformed, exact ordered raw bytes, caller-owned temporary-directory lifetime, unchanged legacy convenience behavior, and actual Reader/Inherit/status physical return paths. Fake-drrun component results do not establish native DR/RPC or H cleanup/evidence precedence. Existing ignored tests remain uncredited; actual counts must come from results, not older populations. Unchanged shared RPC transport tests are not added to this slice.

H locked/offline metadata is ordinary dependency readiness. Its FIRST test/build check is default-feature Hermit lib/bins, including unit-test cfg compilation:

- `cargo nextest run --offline --locked --cargo-profile validate -p hermit --lib --bins -j40`
- `cargo nextest run --offline --locked --cargo-profile validate -p hermit --features dbt --lib --bins -j40`
- Original eleven exact CLI selectors, `--features dbt --test cli -j1`, validate profile, `HERMIT_REQUIRE_DBT=1`; full literal expression will be bound in the new command record (names below).
- `HERMIT_REQUIRE_DBT=1 cargo nextest run --offline --locked --cargo-profile validate -p hermit --features dbt --test inherited_terminal_read -j1`
- ONE DEBUG/default-test-profile Nextest regression command, with no --cargo-profile: `cargo nextest run --offline --locked -p hermit --features third-party-backends,kvm-execution-tests --test procfs_determinism -E 'test(=chroot_mountinfo_subset_keeps_fdinfo_identity_consistent) | test(=chroot_mountinfo_subset_keeps_fdinfo_identity_consistent_with_readonly_proc)'`. No automatic repeat. This preserves incoming shared stack-fix coverage independently of DBT stats.
- `cargo clippy --offline --locked --workspace --all-targets --all-features -- -D warnings`
- `RUSTDOCFLAGS='-D warnings' cargo doc --offline --locked -p hermit --all-features --no-deps`
- `cargo fmt --all -- --check`

Exact CLI11:

- dbt_verify_retains_captures_under_the_names_the_harness_scans_for
- dbt_verify_without_json_rejects_io_buffer_content_divergence
- run_dbt_keeps_diagnostics_out_of_guest_stderr
- run_dbt_recovers_after_failed_exec
- run_dbt_verify_keeps_the_diagnostic_of_a_backend_failure
- run_dbt_verify_returns_with_blocked_stdin_source_after_both_runs
- run_dbt_verify_replays_stdin_bytes_and_eof
- verify_run::paired_verify_runs_keep_logs_and_need_no_run_two_after_a_rejected_run_one
- verify_run::paired_verify_runs_refuse_unlike_runs_and_find_a_difference
- verify_run::paired_verify_runs_report_what_run_verify_reports_on_dbt
- verify_run::paired_verify_runs_report_what_run_verify_reports_on_ptrace

Capture the actual current Release native client/Rust runtime linkage after native CLI checks. Keep terminal DBT's debug logging/strict/minimal-env/fixed epoch distinct from EOF's default logging/strict canonical assertions. No blanket canonical or cross-backend claim from functional totals.

Fresh maintained counts: derive the literal hermit_unit/cli/cli_on_host Nextest LIST selections from the released current DAG, including exact features, skips and hosted filter. Preserve ignored/filtered populations separately; DBT-only executed totals are not maintained count pins. No count/source edit unless a concrete measured mismatch is handed to baseline.

Direct consistency commands, no --write absent a measured mismatch:

- `cargo run --offline --locked -p hermit-manifest-plan --bin generate-validation-dag -- --check`
- `cargo run --offline --locked -p hermit-manifest-plan --bin test-harness -- sync-cells --check`
- `cargo run --offline --locked -p hermit-manifest-plan --bin hermit-manifest-plan -- --format json`
- `CARGO_NET_OFFLINE=true ./scripts/patch-reverie-dbt-buck.rs --check-inventory`

Every actual command gets a fresh immutable argv/environment/source record and separate stdout/stderr/timestamped exit result. Inspect actual summaries and literal exits; a command start or successful compile is not test completion. Retain any setup/compiler/runtime failure unchanged, with a separate successor attempt only if a concrete correction is authorized. Final handback joins exact sources, dependency lock/toolchain, runtime linkage, measured scopes/counts, raw command records and source cleanliness. This is equivalence/refactor regression coverage; no nonexistent-API compile failure will be described as a pre-existing behavioral failing-first bug.
