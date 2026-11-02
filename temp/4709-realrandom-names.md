Exact failing cases from the log investigator (run 20261105T153255Z validation 4, batch #5148/#5173/#5176/#4709 on candidate 8fc27de1e607, base 0220dd857a, donor devbig030; 12,647 executed, 12,641 passed, 6 failed). Failed nodes: privileged-test.pmu_ptrace_completion_cases and test.cli. Report: /home/newton/merge-queue-shadow/musedrain-round2/state/runs/20261105T153255Z/report.md ; run log: /home/newton/merge-queue-shadow/musedrain-round2/state/runs/20261105T153255Z/validations/0004-batch/run.log (lines 35-41).

The five real_random cases (node privileged-test.pmu_ptrace_completion_cases):
1. hermit-run$ptrace_completion::tests::real_random_copy_failure_retires_initialized_sibling_and_timer
2. hermit-run$ptrace_completion::tests::real_random_copy_failure_with_trace_diagnostics
3. hermit-run$ptrace_completion::tests::real_random_live_copy_preserves_sibling_and_timer_success
4. hermit-run$ptrace_completion::tests::real_random_readv_import_failure_retires_initialized_sibling_and_timer
5. hermit-run$ptrace_completion::tests::real_random_readv_live_import_preserves_sibling_and_timer_success

Sixth (timing-sensitive, node test.cli, not yours unless proven): hermit-run::cli$cpu_time_clocks_read_the_logical_cpu_accounting.

History: identical five also failed at 23ea511e (run 20261105T114750Z v5, donor devbig030). At 2f77218f they did NOT fail (that batch was red only on native_config). Withdrawal for 16d90177 is pending-intake since 2026-11-05T16:13:09Z. Note: a NEW batch including 4709 @16d90177 (candidate 1bd9145c216e) was in flight on devbig030 in run 20261105T160713Z v4 — your fix head will supersede it; coordinator resubmits.

Reproduce ONLY node privileged-test.pmu_ptrace_completion_cases / these five names at 16d90177 vs main 0220dd857a before changing code.
