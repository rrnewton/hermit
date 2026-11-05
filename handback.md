# Shared DBT statistics capture

The source pair is committed, clean and released for plain Cargo validation. Validation is in progress; the complete result/report join is pending. This is not a publication or landing claim.

| Repository | Fixed base | Candidate | Local source |
| --- | --- | --- | --- |
| Reverie | `e1ecaeb31a0bb1f8b86f36e769296ab6a7de3782` | `ff30c5bac6040c14122d33d60f5dfb91953c7099` | [reverie-stats-capture](reverie-stats-capture) |
| Hermit | `55f75cbaf7db7b2715f639c14d09a508d9a78932` | `342daf53c1691da42ae7067850540a06bec504b4` | [hermit-stats-capture](hermit-stats-capture) |

Both repository stacks use `hermit-bots <341730520+hermit-bots@users.noreply.github.com>` as author and committer. Their commit bodies begin with the fresh, literal `who-am-i` disclosure and explain impact, determinism, Linux semantics and the validation boundary. Hermit's ownership change is `e7307c2c2aa7f97d0f4aa209ea47122d0915cfc5`; its one-line test-import successor is `342daf53c1691da42ae7067850540a06bec504b4`. No push or full validation was performed by this source worker.

## Ownership and compatibility

Reverie's existing launcher now exports `DbtStatsSink`, which owns the temporary directory, native client argument and completed file read. `DbtStatsCapture` preserves missing, unreadable, present-empty and malformed artifacts separately, borrows their original bytes, and uses the existing typed codec. The legacy optional statistics API still maps a missing file to an empty snapshot.

Hermit removes its duplicate temporary-file owner, client argument construction, file read and aggregation. It retains strict verification policy, the original outer allocation/completion points, caller TMPDIR, `hermit-dbt-verify-stats-` prefix, `records.bin` leaf and existing bind declaration. Reader, inherited-input, status, process cleanup, physical result serialization, complete image/FINAL validation and latch/status precedence are unchanged. Saved runs use the exact captured records through the existing writer and shared reader, without re-encoding totals or rereading the live file.

No new native record format, protocol, dispatch measurement, scheduler policy, descriptor adoption or deferred execution API is introduced. This is not a claim that DBT implements a physical `DispatchStats` route. D2 and shared protected log transport remain separate slices.

The discarded inside-container/raw-IPC draft is retained under [draft-in-container-superseded](draft-in-container-superseded). It was never compiled or run. The active change avoids its custom-TMPDIR visibility difference.

## Exact source scope

Reverie changes four paths: `backend_stats.rs`, `launcher.rs`, `lib.rs`, and the new `launcher/tests/stats_capture.rs`. Hermit changes two consumer/test files, thirteen uniform manifest/lock/Buck pin files, two SDK budget history/active-pin scripts, the Reverie gitlink and the single DBT inventory declaration. Native build recipe inputs and numeric budget limits are unchanged. The new test module changes actual tracked `reverie-dbt` enumeration from 952 to 953 files; this is source enumeration, not yet a named inventory-script pass.

Hermit's existing whole-tree test was extended, with no new test count. Six new Reverie tests cover presence, deterministic read errors, exact ordered binary records, caller-owned lifetime/cleanup and the old Reader/Inherit/status routes. Their fake launcher fixtures assert host-side output/Global return and capture ownership; execution is pending, and those component fixtures cannot establish native DynamoRIO execution or Hermit Global cleanup coverage. The separate real CLI checks must join the actual candidate's native linkage and each fixture's precise assertions.

Precommit receipts are [reverie-precommit-review.json](reverie-precommit-review.json), [hermit-precommit-review.json](hermit-precommit-review.json) and [pin-source-update.json](pin-source-update.json). The SDK notes preserve old history and explicitly avoid credit for old client/runtime artifacts. No Hermit selection, expected-count or generated DAG change is presently needed or claimed.

## Validation

Direct pinned rustfmt checks and `git diff --check` passed before commit. Independent source review found no remaining production blocker after restoring the original placement boundary. The final test source and saved-record assertion were separately reviewed.

The first DBT lib/bin build at `e7307c2c2aa7f97d0f4aa209ea47122d0915cfc5` failed with E0433: its existing test-only `dbt_stats` helper still used `DbtBackendStatsAggregator` after the production import was removed. **Exit 101, no DBT tests executed** is retained in [h-hermit-dbt.stderr](validation-plain-stats-capture/h-hermit-dbt.stderr) and [h-hermit-dbt.result](validation-plain-stats-capture/h-hermit-dbt.result). The default lib/bin run at that parent had passed 895/895; it is not relabeled as a successor run. Commit `342daf53c1691da42ae7067850540a06bec504b4` adds exactly one local import inside the already `cfg(feature = "dbt")` helper. It changes no production code, assertions, feature gates or selection. The [precommit receipt](hermit-test-import-precommit.json) retains that exact patch. Pinned rustfmt and whitespace checks pass; the identical DBT suite is released for rerun, with no successor pass claim yet.

The executor owns the exact commands/results under the [stats capture check plan](stats-capture-check-plan.md). Focused capture and strict-evidence tests precede default Hermit lib/bin execution, DBT execution/verification cases, relevant documentation, inventory and the mandatory workspace/all-targets/all-features Clippy command. An absent new API on the old base is not described as a failing-runtime regression: this slice's claim is behavior-preserving ownership consolidation.

Default-feature test execution is required: `cargo nextest run --offline --locked --cargo-profile validate -p hermit --lib --bins -j 40`, with no feature flag. Default production Clippy does not compile the same test harnesses, and all-features Clippy cannot replace this configuration. Default, DBT-only and maintained-feature populations remain distinct. The focused checks are not a fresh parity cohort or a measurement of flake rate, reliability or broader backend coverage.

The finite core-count audit reads the actual ff30 command from [commands-r.json](validation-plain-stats-capture/commands-r.json): `cargo test --offline --locked -p reverie-core --lib -- --test-threads=1`. Its [stdout](validation-plain-stats-capture/r-core-full.stdout) records **112 passed, 0 failed, 0 ignored, 0 filtered out**, and its [result](validation-plain-stats-capture/r-core-full.result) records exit 0. This is the library unit-test target; it does not execute integration or documentation tests. The candidate changes no `reverie-core` source or manifest, and that package declares no feature selection. The coordinator's separate **124/124** report for landed e1eca has no retained command/raw report available in this bounded audit. Its scope is therefore unjoined: do not equate it with these 112 unit tests or infer test loss, feature differences or a specific arithmetic explanation.

The remaining execution results and complete named-check report join are **pending**. Source is frozen while the executor works. The source planning record is [source-plan.md](source-plan.md), and the independent design/source review is [architecture-review.md](architecture-review.md).
