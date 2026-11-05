# Shared DBT statistics capture: fixed-main restack

This clean, local-only and unpublished pair is ready for coordinator publication with all agreed plain Cargo checks complete at the exact heads below. All 24 recorded Cargo/helper commands exited 0, and both sources plus 15 populated submodules closed clean with unchanged locks. The parked H342daf/Rff30 pair and its original evidence remain intact; none of its results is credited to these new heads.

| Item | Exact identity |
| --- | --- |
| Hermit candidate | `d09ef1a3e7f46a5206e464b562a22c2e77ae9699` |
| Hermit fixed base | `7f53641d93b2332b1ce53f1b97026cd0d32cb9f1` |
| Reverie candidate / Hermit pin | `dde3f9da33a0474d0fa9b273a25100fc0db4d751` |
| Reverie fixed base | `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d` |
| Preserved Hermit source | `342daf53c1691da42ae7067850540a06bec504b4` |
| Preserved Reverie source | `ff30c5bac6040c14122d33d60f5dfb91953c7099` |

The Hermit base is the locally observed coordinator `origin/main` object containing the landed stack fix. Its pin is the selected Reverie base. The independently observed Reverie `main`/`origin/main` refs were stale ancestors, so no fresh remote-tip or publication claim is inferred from them. No network fetch, push or full validation was performed by source integration.

## Source providers and change

Local fetch/source providers are:

- Hermit: `/home/newton/work/dev-hermit/worktrees/slots/dbt-lane-20261010/ignored/dbt-package-d-next-v1/hermit-stats-capture-main7f`
- Reverie: `/home/newton/work/dev-hermit/worktrees/slots/dbt-lane-20261010/ignored/dbt-package-d-next-v1/reverie-stats-capture-main16ff`

Both commits use `hermit-bots <341730520+hermit-bots@users.noreply.github.com>` for author and committer, a fresh literal who-am-i disclosure on the first body line, and substantive public-safe summary/determinism/Linux/validation sections. Commit messages contain no private filesystem paths. The new worktrees were created under the owned slot using existing local objects/imports; the parked heads and source files were not rewritten.

Reverie changes exactly four paths: `reverie-dbt/src/backend_stats.rs`, `launcher.rs`, `launcher/tests/stats_capture.rs`, and `lib.rs`. Their after-images are byte-identical to the reviewed parked source. The public shared sink owns allocation, native client configuration, reading and decoding; the captured artifact distinguishes missing, present-empty, unreadable, malformed and valid raw records. Existing optional Reverie projection still permits missing-to-empty snapshot behavior. No physical-dispatch measurement, wire-format change or new transport route is introduced.

Hermit changes exactly 19 paths: the two reviewed consumers (`backends.rs`, `verify_run.rs`), 13 manifest/lock/Buck pin files, two SDK-budget notes/guards, the Reverie gitlink and one inventory constant. The two consumer after-images equal the reviewed parked successor, including its cfg-local test import correction. Incoming dependency/features, SDK history/numeric budgets and the full `lib.rs` output-future stack repair are preserved. This is one fresh commit per repository; the old import failure/repair history remains in the old source/evidence pair.

## Behavioral invariants and source checks

Hermit retains its existing outer temporary-directory allocation boundary, TMPDIR parent, `hermit-dbt-verify-stats-` prefix, `records.bin` leaf, thin container-bind declaration and caller-owned lifetime. Capture stays at the existing host completion boundary. No new raw-record IPC or Reader execution API is used. Reader streaming/cancellation, inherited input, status, Global cleanup and primary run errors preserve their old order. Protected evidence and latch refusal still precede strict statistics policy; no missing, empty, malformed or inconsistent artifact becomes an accepted empty measurement. Saved `DBT_STATS` bytes remain exact, including ordered unequal binary records; offline reads use the same shared codec while preserving strict policy.

[Source consistency join](stats-restack-main7f-source-join.json), SHA256 `35fe20f2773ac27413598c7c991eb24d88a0452645520d2fd8a62565e024c9bc`, records:

- All 13 pin-only files invert exactly by substituting the new Reverie SHA back to the base SHA. The gitlink and both active SDK guards name the new SHA. The only remaining old full pin occurrences are the two preserved historical SDK comments.
- All seven recorded SDK input Git objects are equal between R16ff and Rdde3: build script, vendored DynamoRIO tree, DBT manifest, native CMake, third-party tree, root manifest and toolchain. This carries source-recipe eligibility/budgets; it is not a new SDK timing result or permission to credit an old native client/runtime.
- Actual tracked-path enumeration is 952 to 953, adding only `reverie-dbt/src/launcher/tests/stats_capture.rs`; the Hermit inventory constant is 953. The source join is a tracked-path enumeration; the current maintained inventory command independently confirmed 953 during validation.
- The stack-fix file, generated DAG and static count source remain exact base bytes. The configured maintained counts are unit 1041 (both twins), CLI 385 and hosted CLI 321; fresh matching listings independently confirmed these unchanged pins. They are enumerated populations, distinct from the executed feature scopes below.

The finite [independent source review](architecture-review.md) found no new source blocker or policy relaxation. It confirms the same minimal capture-owner design and preserved allocation/bind/cleanup/evidence order. Neither this helper extraction nor its pin companion changes a Tool/Guest/Backend abstraction or scheduling/determinization policy; the earlier inherited-input changes remain separate.

## Maintained DBT cell selection (configuration only)

At both fixed H `7f53641d93b2332b1ce53f1b97026cd0d32cb9f1` and candidate H `d09ef1a3e7f46a5206e464b562a22c2e77ae9699`, `ci/compat-envelope/cells.json` schema 9 has **206 selected comparable DBT cells**: 205 portable and one privileged, all declared in `verify` mode. Its complete DBT configuration population is 1,794 unique cells: 206 selected (`green`), 47 applicable but unselected (`red`), and 1,541 not applicable. These status names describe gating selection, not passing or failing runs.

`ci/expected-e2e-plan.json` selects **207 unique required DBT regression rows**: those same 206 comparable cells plus the portable custom command `c-programs/io-uring-fallback`. The custom command is outside the comparable cell denominator. Both sets were joined by the maintained identity `(lane, category, test, mode, backend)`, with no duplicates.

This interpretation follows `ci/compat-envelope/scorecard.rs:243` (selection is not a test result), `:799` (`green`/`red` labels) and `:4873` (comparable/custom partition). Base, candidate and working-file bytes are identical for all three inputs:

| Maintained input | SHA256 at both heads |
| --- | --- |
| `ci/compat-envelope/cells.json` | `c2940ac5f48d4664f296252fbf5381edcad5c782c2fd5bf455175a751afcd9ce` |
| `ci/expected-e2e-plan.json` | `d54ed4775279273f3121fdf16baee0dda9c43f5551ed2d338757542903ae7638` |
| `ci/compat-envelope/scorecard.rs` | `1304b0513c739ad990ff61e558ebc1ff0ed96b0361406c033048c651a62a8726` |

This is a read-only count of unchanged committed configuration. It does not refresh any canonical runtime result, cohort or flake-rate measurement; no census, scorecard writer, cell flip or generator was run. Current focused execution is reported separately below; configuration selection remains 206 to 206.

## Completed validation at the new pair

The [closed executor report](validation-plain-stats-capture-main7f/report.md), SHA256 `ca6347cc56c8dadee739b04338eca3816569fc84322ebc7f512ff10a5aac1b48`, joins all 24 literal successful Cargo/helper exits. Its [evidence index](validation-plain-stats-capture-main7f/evidence-index.json), SHA256 `05f266b0b7332f94542495d663373a6020b4be341d0bbbb9bd0dce09f64e14e5`, authenticates the raw records. [command-manifest.json](validation-plain-stats-capture-main7f/command-manifest.json) contains exact commands, environments, start/end times and sources; [source-after.json](validation-plain-stats-capture-main7f/source-after.json) closes both exact heads, locks, toolchains and all 15 populated submodules clean. No source or generated file changed during this validation.

| Executed scope | Profile/features and actual result |
| --- | --- |
| R focused capture tests | Default Cargo test profile; 6 passed, 153 filtered |
| R core lib | Default Cargo test profile, `-p reverie-core --lib`; 112 passed, none ignored |
| R DBT lib | Default features; 144 passed, 15 inherited ignored |
| R DBT lib, no default features | 137 passed, 15 inherited ignored |
| H default-feature lib/bins | `validate` profile; 896 passed, zero skipped; ran before DBT |
| H DBT lib/bins | `validate`, feature `dbt`; 936 passed, zero skipped |
| H live/saved/offline CLI | Exact existing 11 selectors, `validate`, feature `dbt`; 11 passed, 407 outside selection |
| H inherited-terminal target | `validate`, feature `dbt`; 6 passed: three DBT cases and three native Linux controls |
| H ptrace procfs stack regressions | One DEBUG Nextest round, features `third-party-backends,kvm-execution-tests`; both named chroot cases passed, 49 outside selection |

The six focused capture tests are included in the full R DBT suite and are not added to it again. The 112 core tests are this exact unit scope; no equivalence to a differently reported coordinator population is inferred. Fake-launcher component tests prove presence/read/decode distinctions, exact bytes and caller-owned capture lifetime across Reader/Inherit/status returns; they do not establish translated DR or RPC execution.

The report records all 11 CLI names and their exact invocations. In particular, the unchanged DBT `/bin/cat` byte/EOF fixture passed with default logging, `--strict --verify --verify-strict --verify-json`, and no determinism relaxation. Its assertions require the exact input bytes, successful exits, matched canonical verdict, `bitwise_parity=true`, I/O-buffer comparisons and nonzero compared INFO counts. Existing fixture cleanup removed the temporary verdict JSON, so no literal INFO count is asserted here. This L2 evidence is specific to that exact guest and fixture; the functional totals are not a blanket canonical or cross-backend claim. The three DBT terminal cases use debug logging, strict mode, minimal environment and the fixed epoch; their native controls are Linux controls, not a Hermit backend. The DEBUG procfs pair preserves the landed ptrace stack-fix assertions and default test stack.

R focused all-target Clippy passed for `reverie-core` and `reverie-dbt` with `-D warnings`. H's mandatory **workspace, all-target, all-feature Clippy** passed with `-D warnings`. There is **no separate current default-feature Clippy claim**: that configuration is covered here by the 896 executed default-feature lib/bin tests. R docs for both packages and H all-feature docs passed with warnings denied; formatting passed in both repositories. Reverie used nightly 2026-07-29 and Hermit nightly 2026-07-12.

[Current production linkage](validation-plain-stats-capture-main7f/production-linkage.json) binds the rebuilt current native Release client, Rust `libdetcore_dbt.so` and CLI at the real DBT runtime boundary. The unchanged SDK recipe enabled cache reuse; no fresh SDK-build timing is claimed. Maintained-feature listings later rebuilt the shared CLI path, explicitly recorded by [linkage-after-listing.json](validation-plain-stats-capture-main7f/linkage-after-listing.json); the other four artifact hashes stayed equal. The later CLI compile does not replace the earlier captured runtime identity.

Fresh [maintained listings](validation-plain-stats-capture-main7f/maintained-list-summary.json) match the unchanged count pins:

| Maintained selection | Selected nonignored | Filtered nonignored | Ignored, also filtered | Enumerated |
| --- | ---: | ---: | ---: | ---: |
| Unit: `third-party-backends,kvm-native-test-support` | 1041 | 5 | 0 | 1046 |
| CLI: `third-party-backends` | 385 | 101 | 5 | 491 |
| Hosted CLI: same features and maintained hosted filter | 321 | 165 | 5 | 491 |

These are list-only populations, not 1041/385/321 executed tests. Original features, skip lists and hosted filter remain unchanged; no ignored case is credited. The unchanged shared Detcore declaration 1581 was not remeasured in this scope. No count edit or generated-DAG write was necessary.

All four direct consistency helpers passed: DAG `--check`, `sync-cells --check`, manifest JSON emission/parsing and the maintained DBT inventory checker (953 files). DBT cell selection remains **206 to 206**, with no cell flip, scorecard write or fresh census.

Two local module-population refusals and an initial linkage-inspection assertion are retained in the executor report. They preceded or were separate from product checks and receive no test credit. No compiler, test, lint, docs or helper command failed on this released pair. The older parked-pair cfg import failure and earlier D3/stack failures remain historical evidence at their original heads; they were not overwritten or relabelled.

## Resources and coordinator handoff

The single executor used nice 19, CPUs 0–39 and at most 40 build jobs, with existing protected caches. Initial free space was 781,304,754,176 bytes; subtracting the conservative 64 GiB estimate left 712,585,277,440 bytes. Closing free space was 773,270,581,248 bytes. All recorded boundaries exceeded the 650,000,000,000-byte build floor. Actual attributable incremental footprint and peak use were not measured; host free-space deltas are not task allocation measurements.

No native ABI, RPC protocol, shared dispatch route or guest-visible placement contract changed. Publish the Reverie commit first, then have the coordinator validate and publish the Hermit companion against the published dependency. These slot providers contain one clean commit per repository; the coordinator owns publication and any necessary landing integration. This handoff does not claim a full validate, online published-pin check, PR, landing, fresh cohort or flake-rate measurement. No source, count or test work remains in the agreed local scope.
