Follow-up on dh-jqhfx @ 3070c66b06e74a38ab39d39c70723629f5fc25ae (PR 5182). Independent verification passed claims 1-5 (diff scope, code paths, push read-back, 25/25 xstate_probe tests, concurrent ptrace CLI pair exit 0 with one shared measurement). One decisive gap remains: count pins.

Your change adds exactly 5 #[test]s (xstate_probe 11 -> 16 in mod.rs; nextest xstate total 20 -> 25). Pins unchanged: test.hermit_unit=1268, test.hermit_unit_on_host=1262 in ci/manifest-plan/src/validation_dag_static.rs, trail comments still say "eleven xstate_probe tests". As-is the PR fails the executed-count gate.

Fix on the same branch:
1. Compose pins as main + (head - base) = +5: test.hermit_unit 1268->1273, test.hermit_unit_on_host 1262->1267. Prefer the repo's recount tooling if present (search ci-hub/bin and scripts for recount-pins / rebase-drain); never hand-sum anything beyond this measured delta and never pass --allow-drop. Update both trail comments eleven->sixteen.
2. Regenerate ci/dag/validate.json with the DAG generator (do not hand-edit it).
3. Verify: generate-validation-dag --check canonical; cargo test -p hermit-manifest-plan passes; confirm the 5 new tests are SELECTED in the hermit_unit node selection.
4. cargo fmt --all -- --check clean.
5. New commit (repo AGENTS.md format, Test plan in body), push via herdr-run with --force-with-lease=refs/heads/fix/dh-jqhfx-probe-lock:3070c66b06e74a38ab39d39c70723629f5fc25ae, uncredentialed ls-remote read-back. PR 5182 updates automatically; no new PR, no body edit unless the Validation section needs the new head noted.
6. Report DONE with the new exact 40-hex head and the verification results. Still not landed / not submitted.
