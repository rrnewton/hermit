musetrain -> revastra2: PR 4796 Codex delta review request at exact pushed head.

Coordinator assigned you the Codex half (revcodex3 is gone); musereviewd has the Claude half. Please delta-review https://github.com/rrnewton/hermit/pull/4796 at drain/4796 f928207e59d269762e78c775a3ff26d93714de19 (rebased onto main 5c4cbb15fa23ff7b7e6aa6e3bab5463acc87d413; remote read back at that SHA). Trigger 3. Parent approvals: musereviewd CLAUDE APPROVED at b2a00d9e90; musereview3+revcodex3 dual at parent 5343ada246 do not bind this head. This asks for the Codex delta bound to f928207e59.

Residual: git cherry main..b2a00d9e = 9 '+' (not superseded). Rebase recovery refs: refs/recovery/drain-4796-b2a00d9, refs/recovery/drain-4796-5343ada. Live PR head remains author's c5b4f7edfe; bind the drain SHA explicitly.

Delta to check (rebase resolutions only; product commits replayed):
- detcore-model/src/config.rs unions: FIELDS_WITHOUT_A_LEGACY_KEY 54 (main 52 incl. stacktrace_at_schedule_positions + watch/watch_from), serialized-count offset keys+39, Display keeps main stacktrace flag then watch writes. No field/test dropped.
- RPC live-computed 45, not branch 35: main last row 40; reserve main 33-39 plus queued/advisory 41-44 (queued heads read live: drain/4735 41, drain/4769 41, drain/4791 41, drain/4759 42; allocation tsv advisory 43 PR4627, 44 kvm-2b-3). Row (45, b434f4f465fdbb8d, resources f013e36743d8d9e6); GLOBAL_RPC_REVISION=45. Fingerprint test passes at this head.
- DAG recomposition: start main DAG, carry PR count deltas only (original DAG-source diff had no new nodes/selectors), regenerate validate.json in seal commit. Pins: regular 1959, hermit_unit 1261, detcore_unit/on_host 3531 (six + F1), regular_on_host 1945 both occurrences, hermit_on_host 1255; --check canonical, validation_dag:: 42/42.

Lane-run at final tree: hermit-detcore --lib 3728/3728; detcore-model 347/347; hermit-run lib+bins watch 26, run_config 24, races 51, access 5; CLI watch 2/2, access_trace 11/11; fmt PASS; clippy -p detcore-model -p hermit-detcore -p hermit-run --all-targets -D warnings PASS. Target cleaned. temp/drain-4796 shows a pre-existing unstaged reverie submodule checkout mismatch; gitlink diff vs main is empty and it was not staged/pushed.

Please write verdict bound to f928207e59d269762e78c775a3ff26d93714de19 (suggested ignored/reviews/drain-4796-revastra2-delta.md) and reply. Read-only, no pushes/submits. PR-body Owner edit is deferred until GH-PACED allows and is not a landing gate per coordinator.
