# Default-feature coverage and next D test scope

Read-only planning, 2026-10-14. No compiler, test, dependency preparation, source edit or cache write was performed. The next architecture choice remains with the coordinator; these are proposed checks for the released source, not execution evidence or a new runner.

## Exact repair and the missed configuration

The repair is locally available as Hermit **55f75cbaf7db7b2715f639c14d09a508d9a78932**, parent **ff95b21adbbe4b36f300431a3e142338da0511b9**, with Reverie gitlink **e1ecaeb31a0bb1f8b86f36e769296ab6a7de3782**. These fixed objects are the planning anchor. The local parent repository's cached `origin/main` was observed at abfc6d98aa60a1dedbfeb717092fd21ec6ec61ed and contains the repair; that is offline ancestry evidence, not a fresh publication query or a new target. Its working HEAD was separately 25781c5c48d47c316324fd912b7dcafa394c339a.

The repair changes exactly one line in `hermit-cli/src/bin/hermit/run.rs`: it adds `#[cfg(feature = "dbt")]` to `dbt_rejects_mount_but_accepts_bind_and_workdir` (line 2727 at this anchor). `RunOpts::dbt_binds` already has the same guard (line 7866). The method and every assertion are unchanged. `hermit-cli/Cargo.toml` declares `default = []`; `third-party-backends` includes `dbt`.

Our retained default check at H c4668d56ec85048db61894e8dc382664107922da was:

```text
cargo clippy --offline --locked -p hermit --lib --bins -- -D warnings
```

It returned 0, as recorded in `../dbt-shared-verify-d3-v1/validation-plain-main199d/{commands-h.json,h-clippy-default.result,h-clippy-default.stderr}`. It selected ordinary library/binary targets, not their `cfg(test)` harnesses. The unconditional `#[test]` function therefore did not compile in that default-feature check. Our all-targets/all-features Clippy did compile the test, but also enabled the DBT method. The DBT and maintained third-party unit suites likewise enabled DBT. Neither successful shape established that **default features plus test compilation** worked. This was a coverage gap; the earlier green checks are retained with their actual scope.

The repair commit reports `cargo nextest run -p hermit --lib --bins` passing 895/895. That is the commit author's reported result; this planning task has not independently joined its raw run or executed it. No old DBT-only or maintained-feature count is substituted for the future default population. The DBT-specific test remains selected under DBT/third-party features, so this cfg repair does not reduce that maintained population.

## Permanent hand-back shapes

Use each repository's checked-in nightly, the existing ordinary Cargo home/target, nice 19, affinity at most 40 CPUs, Cargo/CMake jobs 40, and no competing Cargo owner. For Hermit unset the R-only short socket `TMPDIR`. The commands below use the existing `validate` test profile and default Nextest configuration. Keep exact head, features, selected/executed/ignored/filtered counts and literal results; do not add skips to make a new failure disappear.

1. **Default Hermit lib/bin execution:**

   ```bash
   cargo nextest run --offline --locked --cargo-profile validate \
     -p hermit --lib --bins -j 40
   ```

   This deliberately supplies no `--features`: it compiles and runs the default test harnesses. If a compile-only stage is useful before execution, its actual equivalent is `cargo test --offline --locked --profile validate -p hermit --lib --bins --no-run`. A successful Nextest run already includes that compilation; do not require a redundant build. Compile-only success must not be reported as test execution.

2. **DBT Hermit lib/bin execution when DBT/common verification is affected:** `cargo nextest run --offline --locked --cargo-profile validate -p hermit --features dbt --lib --bins -j 40`. Retain the real live/saved/offline DBT verification selectors, original byte/EOF test and applicable terminal/fork/exec regressions. Default execution is additional coverage, not a replacement for these.

3. **Mandatory cross-feature lint:**

   ```bash
   cargo clippy --offline --locked --workspace --all-targets --all-features -- -D warnings
   ```

   Keep this exact target/feature scope for R API changes and D hand-backs. Default `cargo clippy -p hermit --lib --bins -- -D warnings` remains useful for ordinary default dead-code/production cfg warnings, but never substitutes for item 1. All-features lint also never substitutes for default test compilation.

4. **Finish the actual source scope:** `cargo fmt --all -- --check` in each changed repository; relevant changed-package rustdoc with warnings denied; maintained-feature Nextest listings and direct count/DAG/sync/inventory checks only where the test population or source inventory changes. Default, DBT-only and maintained third-party populations must remain separately measured. No fullvalidate/composer/admission machinery is introduced.

## Remaining shared statistics / transport: existing test boundaries

The prior source plan is `../dbt-shared-verify-d3-v1/d3a-shared-transport-plan.md` (original H a46/R b866 anchor), independently reviewed beside it. Its common-verifier move has since landed; its proposed protected-output attachment is not assumed present merely because the host loop is now shared. The following target inventory was checked at fixed H55f75/R e1ec. Source design and any T-owned API changes remain separate decisions.

| Changed responsibility | Existing target and meaningful assertions |
|---|---|
| Shared typed `DispatchStats` projection | R `reverie/src/dispatch_stats.rs`, package `reverie-core --lib`. The existing `dispatch_stats::tests` cover trapped versus direct routes, unknown counters distinct from zero, JSON null retention, per-process sum mismatch, partial attribution and duplicate process IDs. A pure projection slice can start with this Rust component selection without DBT being enabled. Add a regression for the actual new representation/consistency rule, then run the relevant full core library suite. Do not rename Tool-call counts into a physical delivery route or fill unknown values with zero. |
| DBT snapshot/wire aggregation and launcher completion | R `reverie-dbt/src/backend_stats.rs`, `launcher.rs`, `evidence.rs`; retain complete `reverie-dbt --lib` default and supported `--no-default-features` shapes when these change. Existing tests cover missing/truncated stats, tree aggregation, exact FINAL, duplicate FINAL, backend failure before missing root FINAL, retry sequence equality and denied publication. Default library runs leave explicitly ignored real-native tests uncredited. A changed native transport/ABI requires its real production client fixtures, not just decoder mocks. |
| H statistics and common verdict consumption | H `hermit-cli/src/bin/hermit/backends.rs`, selected with `hermit --features dbt --lib --bins`. Existing tests include `typed_stats_capture_aggregates_the_whole_process_tree`, `typed_stats_capture_refuses_missing_empty_or_truncated_evidence`, `dbt_stats_block_labels_counters_honestly`, `dbt_stats_block_renders_the_typed_wire_snapshot`, `dbt_summary_compares_every_non_branch_observation`, `missing_dbt_completion_stats_are_attributed_to_the_completed_run`, and both common-verifier failure-precedence tests. Keep saved/offline callers and actual runtime verdict tests; a display-only result does not establish native reporting. |
| Counted Detcore INFO forwarding | H `detcore/src/detlog.rs`, package `hermit-detcore --lib --bins`. Existing tests exercise per-thread counts, missing/narrowed required forwarder, missing records as loss, full socket buffer progress, schedule-fixed write points, sender tags and loss latches. These protect the actual shared owner; any migration must prove one copy of each record and preserve initialization/other INFO coverage separately. |
| Shared log transport/lifecycle, if actually edited | R `reverie-rpc-transport --lib --tests`, including guest-log and split-capture lifecycle integrations. Retain `split_stalled_serializer_requires_outer_bound`, whose actual serializer is intentionally bounded at 50 seconds. Do not replace this full transport selection with only fast library tests. This is conditional on changing/consuming that transport, not an extra transport rebuild gate for a pure stats projection. |
| Protected descriptor adoption | R `reverie-inguest/src/guest/protect.rs` owns reservation, relocation, close-range preservation and retirement. It has no inline `protect::tests` module at this anchor, so do not invent that filter and accept zero tests. A real DR attachment needs the API owner's applicable tests plus actual DBT close/dup/reuse/fork/exec and writer-retirement coverage. Calling reservation alone does not prove DR protection. No new guest/adapter framework is proposed here. |

The smallest statistics-first selection is therefore the actual core projection tests, followed by the affected DBT snapshot and H rendering/verdict tests if the adapter changes. Keep the default H test shape and mandatory all-features lint in its hand-back even if the initial unit regression is feature-independent. Full shared-transport and protected-native tests become necessary when that behavior is actually migrated; architecture review determines that boundary before deletion.

Physical guest status and evidence completeness remain different facts. At R e1ec, `combine_status_and_evidence` still formats `status.code()` into an evidence error, and `DbtFailedRunOutput` retains streams/error without raw status. Existing H normal-exit/signal tests do not prove status preservation on that combined evidence-failure path. Any future status repair needs observed-status/failure-stage tests through the common run-labelled owner, with missing FINAL/INFO remaining `no_result`; do not synthesize counters or completion, relax the canonical comparator, or absorb parked #4148 into a statistics-only change.

No commands above were executed by this task. All former default-coverage and runtime failure evidence remains intact. This note changes no product, source count, test assertion, dependency, cache or published state.
