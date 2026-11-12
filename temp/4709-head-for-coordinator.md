coord: two items.

1) PR 5182 Test plan: CONFIRMED ADDED. Live body now 4100 bytes with a **Test Plan** section (who-am-i, Owner, Plain Language Summary, Determinism, Linux Semantics, Validation, Test Plan); head unchanged f246147df3d9eaaa6460284605cd288e4eb53a99. Body-only edit via the paced wrapper, read back.

2) 4709 real_random fix ready for resubmission.
New head: c4adf5a36df4f163f043fbae2ab950cfdad6f7a3 on drain/4709 (child of 16d901776f; uncredentialed ls-remote read back exact; author branch untouched).

Cause (your 11:22 extraction confirmed by local repro): the five cases spawn the ptrace tracer directly (hermit_run_ptrace::spawn_tracer) with Config::default(), bypassing library resolve_auto, so the new auto default reached Detcore unresolved -> exit 122. Local repro: main 0220dd857a 5/5 PASS, head 16d90177 0/5 with the identical refusal. Culprit a62d82781d8 (auto default) exposing a latent missing-resolution seam -> fix-forward.

Fix: one file, test code only (ptrace_random_failure_tests.rs +9): names cpu_profile X86_64V2 in that direct launch config (guest declares v2), mirroring the 0065248639/16d90177 seam fixes. Detcore's unresolved-auto refusal untouched; no assertion weakened.

Independently verified: diff exactly +9/-0 one file; all five tests trace through the single fixed Config and no other direct launch in the searched scope carries unresolved auto; re-runs: exact five 5/5, rdrand_rdseed_is_masked 1/1, native_config 9/9; prior fixes in ancestry. Worker also: hermit-inguest-engine 41/41, admission slice 24/24, detcore cpu-profile slice 40/40, cpu_time sixth case 1/1 (launches via CLI, not this seam), fmt/clippy clean, DAG canonical. Failing-test fix, no re-review (owner 5795). Full profile = your resubmission.

One note: merge-tree of c4adf5a3 vs fresh main 0220dd857a conflicts ONLY in the generated pair (ci/dag/validate.json, validation_dag_static.rs); everything else auto-merges. Queue composition/DAG regen owns that as with 4796 — tell me if you want a pre-rebase instead.
