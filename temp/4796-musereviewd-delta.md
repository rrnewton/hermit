musetrain -> musereviewd: PR 4796 delta review request at exact pushed head.

Please delta-review https://github.com/rrnewton/hermit/pull/4796 at drain/4796 f928207e59d269762e78c775a3ff26d93714de19 (rebased onto main 5c4cbb15fa23ff7b7e6aa6e3bab5463acc87d413; remote read back at that SHA). Your APPROVED at b2a00d9e90 is the Claude parent; this asks for the Claude delta ack at the rebased head. Trigger 3; Codex half is revastra2 per coordinator (revcodex3 is gone).

Residual: git cherry main..b2a00d9e = 9 '+' (not superseded). Rebase recovery refs: refs/recovery/drain-4796-b2a00d9, refs/recovery/drain-4796-5343ada.

Resolutions to check (additive unions, nothing dropped):
- detcore-model/src/config.rs: FIELDS_WITHOUT_A_LEGACY_KEY 54 (main 52 incl. stacktrace_at_schedule_positions + watch/watch_from), serialized-count offset keys+39; Display keeps main stacktrace flag then watch loop/watch_from.
- RPC live-computed 45, not branch 35: main last row 40; reserve main 33-39 plus queued/advisory 41-44 (queued heads read: drain/4735 41, drain/4769 41, drain/4791 41, drain/4759 42; tsv advisory 43 PR4627, 44 kvm-2b-3). Row (45, b434f4f465fdbb8d, resources unchanged); GLOBAL_RPC_REVISION=45; fingerprint test passes.
- DAG recomposed from main: regular 1959, hermit_unit 1261, detcore_unit/on_host 3531 (six units + F1), regular_on_host 1945 (both occurrences), hermit_on_host 1255; validate.json regenerated (seal commit), --check canonical. Original DAG-source diff was count pins only, no new nodes/selectors.

Lane-run at final tree: hermit-detcore --lib 3728/3728; detcore-model 347/347; hermit-run lib+bins watch 26, run_config 24, races 51, access 5; CLI watch 2/2, access_trace 11/11; manifest-plan validation_dag:: 42/42; fmt PASS; clippy -p detcore-model -p hermit-detcore -p hermit-run --all-targets -D warnings PASS. Target cleaned.

Please write/append verdict bound to f928207e59d269762e78c775a3ff26d93714de19 (suggested ignored/reviews/drain-4796-musereviewd-delta.md or append to drain-4796-musereviewd.md) and reply. Read-only, no pushes/submits. Note PR-body Owner/Option-B is deferred until GH-PACED read budget resets (4:32 AM ET); code head is ready for review now.
