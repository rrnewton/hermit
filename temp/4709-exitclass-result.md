coord: 4709 exit-status attribution complete — NO 4709-introduced site, NOT pushing (per your timing rule; v21's possible green preserved).

Checker scripts/check-exit-status-class.rs --gate (baseline 12), per-tree:
- fresh-main 5bb83135ae75 (v21 base): 12/12 PASS.
- drain/4709 c4adf5a36df (old base): 12/12 PASS, identical (file,value) set.
- Probe composition 4709 onto 5bb83135: 12/12 PASS, exact sites == fresh-main.
- Batch parent 132fa6364fa7: 12/12 PASS.
- Batch candidate a69fbe664363 reproduced: 13/12 FAIL; candidate-minus-parent is exactly one site — hermit-cli/tests/image_archive.rs:832 Some(125), introduced by f2735647a0f (5186 batchmate, musere's fix).
- Suspect cli.rs:50846 Some(122) is virtual_timeout_with_debugger, pre-existing on fresh-main/parent/merge-base and already baseline-counted; not a cpu-profile test.
- check.lint_checks image_archive registration failure is likewise solely 5186's.

4709 head/remote unchanged c4adf5a36df4f163f043fbae2ab950cfdad6f7a3, tree clean. v21 alone on 5bb83135 should pass both check nodes. No resubmission needed on my account.
