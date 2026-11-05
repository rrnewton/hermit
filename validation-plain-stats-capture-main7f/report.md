Statistics capture validation completed at Hermit `d09ef1a3e7f46a5206e464b562a22c2e77ae9699` and Reverie `dde3f9da33a0474d0fa9b273a25100fc0db4d751`. All 24 recorded Cargo/helper commands exited 0. Both source roots and 15 populated submodules remain at their declared heads and clean; no source, lock, count, cell or generated-file edit was made during validation.

Hermit base is `7f53641d93b2332b1ce53f1b97026cd0d32cb9f1`; Reverie base is `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d`. These are fresh results on the released pair, separate from the parked H342/Rff30 statistics records and the landed stack-fix investigation.

[command-manifest.json](command-manifest.json) contains every exact argv, working directory, source ID, explicit environment, command prefix, start/end time and literal exit. Each command's stem names its separate `.stdout`, `.stderr` and `.result` files. [source-after.json](source-after.json) closes the source, submodule, toolchain and dependency-lock joins. [evidence-index.json](evidence-index.json) records the size and SHA256 of every closed file in this evidence directory, including this report.

The commands used each repository's selected nightly: Reverie 2026-07-29 and Hermit 2026-07-12. All used the existing shared Cargo home/target, `nice -n 19 taskset --cpu-list 0-39`, `CARGO_BUILD_JOBS=40` and `CMAKE_BUILD_PARALLEL_LEVEL=40`. Cargo dependency operations were locked and offline. Reverie used the short slot TMPDIR; Hermit explicitly unset TMPDIR. No test stack override, assertion change, new runner, full validation, network operation or concurrent Cargo executor was used.

| Executed scope | Actual result |
| --- | --- |
| R six `launcher::tests::stats_capture` component tests | 6 passed; 153 filtered |
| R `reverie-core --lib`, default Cargo test profile | 112 passed; none ignored |
| R `reverie-dbt --lib`, default features | 144 passed; 15 ignored and uncredited |
| R `reverie-dbt --no-default-features --lib` | 137 passed; 15 ignored and uncredited |
| H default-feature `--lib --bins`, Cargo validate profile | 896 passed; 0 skipped |
| H `--features dbt --lib --bins`, Cargo validate profile | 936 passed; 0 skipped |
| H focused live/saved/offline CLI selection, dbt/validate | 11 passed; 407 outside selection |
| H `inherited_terminal_read`, dbt/validate | 6 passed: 3 DBT and 3 native controls |
| H DEBUG `procfs_determinism` regression selection | 2 passed; 49 outside selection |

H default-feature lib/bin tests ran before the DBT suite, exercising the test cfg that the earlier default Clippy-only check did not compile. No separate default-feature Clippy result is claimed for this pair. The six focused R tests are included in the full default DBT suite and are not added again to its total. Fake-runner component tests establish raw capture, presence/error distinctions and physical-output/global ownership; they do not prove translated DR execution or an RPC exchange.

The eleven exact CLI names were:

- `dbt_verify_retains_captures_under_the_names_the_harness_scans_for`
- `dbt_verify_without_json_rejects_io_buffer_content_divergence`
- `run_dbt_keeps_diagnostics_out_of_guest_stderr`
- `run_dbt_recovers_after_failed_exec`
- `run_dbt_verify_keeps_the_diagnostic_of_a_backend_failure`
- `run_dbt_verify_returns_with_blocked_stdin_source_after_both_runs`
- `run_dbt_verify_replays_stdin_bytes_and_eof`
- `verify_run::paired_verify_runs_keep_logs_and_need_no_run_two_after_a_rejected_run_one`
- `verify_run::paired_verify_runs_refuse_unlike_runs_and_find_a_difference`
- `verify_run::paired_verify_runs_report_what_run_verify_reports_on_dbt`
- `verify_run::paired_verify_runs_report_what_run_verify_reports_on_ptrace`

CLI and terminal selections required DBT availability through `HERMIT_REQUIRE_DBT=1`. The original EOF fixture ran DBT `/bin/cat` with its unchanged embedded byte sequence, default logging, `--strict --verify --verify-strict --verify-json`, and no determinism relaxation. Its successful assertions require one emitted copy of the exact bytes, matched verdict, canonical comparison, `bitwise_parity=true`, I/O-buffer comparison, zero status on both sides, exact output hashes/lengths, and nonzero compared INFO counts on both sides. Temporary verdict JSON was cleaned by the existing fixture; the assertion-backed positive counts are retained through the test result, not as separately retained numeric values. This canonical evidence is specific to that guest/fixture, not a blanket L2 claim for all functional cases.

The DBT terminal fixtures explicitly use `--log=debug`, `--strict`, `--base-env=minimal`, and fixed `--epoch=2026-10-12T07:49:05+00:00`. Their three native controls are Linux behavior controls, not another Hermit backend. The one DEBUG procfs check used `third-party-backends,kvm-execution-tests` and the unchanged default test stack; its exact names were `chroot_mountinfo_subset_keeps_fdinfo_identity_consistent` and `chroot_mountinfo_subset_keeps_fdinfo_identity_consistent_with_readonly_proc`. This covers the newly landed ptrace frame repair in the base without repeating its earlier diagnostic rounds.

[production-linkage.json](production-linkage.json) records the actual runtime boundary after CLI11 and terminal6: current dde3 native source embedded in the CLI, a production Release native client, and its ELF NEEDED/RPATH resolution to the newly built current Rust `libdetcore_dbt.so`. Five artifact hashes and CMake/link inputs are retained. Reuse of the unchanged SDK cache is not claimed as a fresh SDK build. Later maintained-feature listing rebuilt the shared `validate/hermit` path; [linkage-after-listing.json](linkage-after-listing.json) records that change explicitly. The other four linkage hashes remain equal. That later compile does not replace or relabel the earlier runtime result.

R focused Clippy passed for `reverie-core` and `reverie-dbt`, all targets, with `-D warnings`. H mandatory `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed. R docs for both changed packages and H all-feature docs passed with `RUSTDOCFLAGS=-D warnings`; formatting passed in both repos. These checks use the recorded package/feature scopes. Cargo build-script SDK cache-HIT notices remain in stderr; an empty-stderr claim is not made.

Fresh maintained-list results in [maintained-list-summary.json](maintained-list-summary.json) match all current pins:

| Maintained population | Selected nonignored | Filtered nonignored | Ignored | Enumerated |
| --- | ---: | ---: | ---: | ---: |
| Hermit unit, third-party-backends + kvm-native-test-support | 1,041 | 5 | 0 | 1,046 |
| CLI, third-party-backends | 385 | 101 | 5 | 491 |
| Hosted CLI, same features plus exact maintained filter | 321 | 165 | 5 | 491 |

The five ignored CLI cases are also filtered; none is selected. Exact original skip/filter strings are retained in [maintained-list-commands.json](maintained-list-commands.json), joined to current DAG commands and build selections in [maintained-list-source-join.json](maintained-list-source-join.json). These are enumerations, not execution of those full populations. DBT-only runtime totals do not substitute for maintained count pins. The unchanged Detcore declaration 1,581 was not remeasured in this scope.

All four direct consistency checks passed: generated DAG canonical/fresh, expected plan and optional cells current, manifest JSON emitted and parsed, and the maintained DBT inventory checker confirmed 953 files. No generator write or count correction was needed. [helper-summary.json](helper-summary.json) joins their literal outputs and exits.

DBT configuration remains byte-identical to the fixed base: 206 comparable selected cells (205 portable, one privileged, all verify), plus one custom `c-programs/io-uring-fallback` row, for 207 required DBT rows. The existing source owner's [configuration join](../handback-main7f.md) names the maintained schema and hashes. Selected/green configuration labels are not execution results. This task changed no DBT cells and ran no cohort/census/scorecard writer.

Two initial local Git setup refusals were retained separately: an unpopulated historical DynamoRIO provider and an excessively nested alternate provider chain. They occurred before Cargo tests and carry zero test credit. Local population then used the already present exact16ff stack-fix provider; no network, product or lock change was needed. An initial linkage inspection also asserted that the CLI contained the entire dynamically assembled client path literal; that inspection-only assertion was corrected to the actual embedded native source path and CMake/ELF join. It was not a compiler or product test failure. No product compiler, test, lint, documentation or helper command failed on this released pair.

Disk observations are in the three boundary samples and [disk-after.json](disk-after.json). Before the first build, 781,304,754,176 bytes were free; the conservative additional estimate was 64 GiB (68,719,476,736 bytes), projecting 712,585,277,440 bytes remaining. Before H, 794,855,919,616 bytes were free; before maintained lists, 787,044,286,464; at close, 773,270,581,248. Every observed boundary exceeds the 650,000,000,000-byte no-build floor. Actual attributable incremental allocation and peak disk/memory use were not measured. Host free-space changes include other activity and cannot be attributed to this task.

The current pair is clean and all agreed checks are closed. Older parked-pair failures/results and stack-fix evidence remain separate. No full-validation, whole-backend parity, fresh cohort, online pin-publication or landing claim is made here.
