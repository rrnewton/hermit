# HANDOFF

TaskGraph task: `classify-the-21-always-failing-test-ids-for-the-green-bar` (`hermit-125`, `IN_PROGRESS`). No product files have been edited. No validate or focused test is running, so there is no run handle or live log to preserve.

This handoff is committed on local branch `hermit-125/classify-21-handoff-20260827`. Two attempts to push that branch reached no remote state: `github.com` could not be reached because the configured proxy name did not resolve. The remote branch was confirmed absent after each failure. First reachability action after restart: push this local branch and verify the remote SHA.

## Exact trees

- Slot: `/home/newton/work/dev-hermit/w125-classify`
- Branch: `hermit-125/classify-21-handoff-20260827`
- Hermit HEAD for current-tree remeasurement: `fc9b323cfb298063fec4a1fb56f93ec291c36c6c`
- Reverie revision pinned there by `Cargo.lock`: `ab07a89239150df3726a036bee9f5e897893dfc1`
- Source validate Hermit SHA: `a6b0c37648df774d5859aad23a535caf03a6d392`
- Source validate pinned Reverie revision: `ad598995c8018bf17414a92119acfac6c9fd58ee`
- Source run: `validate-hermit-137-a6b0c37648df-1787798057339452591-387681-9a8a881f`
- Source log: `/home/newton/work/dev-hermit/ignored/validate/validate-hermit-137-a6b0c37648df-1787798057339452591-387681-9a8a881f.log`
- Source metadata: `/home/newton/work/dev-hermit/ignored/validate/runs/validate-hermit-137-a6b0c37648df-1787798057339452591-387681-9a8a881f.json`
- Source artifacts: `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-137-a6b0c37648df-1787798057339452591-387681-9a8a881f/`

Recount from `/tmp/stability.py`: 677 test IDs, 1350 attempt observations, 649 always passed, 21 always failed, 7 unstable.

## The 21 IDs

1. `hermit::app_strict_verify$go_goroutines_are_deterministic_under_strict_verify` — 0 pass, 2 fail
2. `hermit::app_strict_verify$go_hello_is_deterministic_under_strict_verify` — 0 pass, 2 fail
3. `hermit::bin/hermit$run::detects_symlink_resolution_through_implicit_mounts` — 0 pass, 2 fail
4. `hermit::cli$every_record_container_site_classifies_a_child_fault_by_name` — 0 pass, 2 fail
5. `hermit::cli$run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls` — 0 pass, 2 fail
6. `hermit::cli$run_dbt_verifies_queued_self_signals` — 0 pass, 2 fail
7. `hermit::cli$run_dbt_verifies_self_prlimit` — 0 pass, 2 fail
8. `hermit::cli$run_dbt_verifies_shell_process_lifecycle` — 0 pass, 2 fail
9. `hermit::cli$run_dbt_verifies_simple_env_shebang` — 0 pass, 2 fail
10. `hermit::cli$run_dbt_virtualizes_process_identities` — 0 pass, 2 fail
11. `hermit::cli$run_liteinst_rejects_a_non_runtime_override_before_activation_claim` — 0 pass, 2 fail
12. `hermit::cli$run_liteinst_rejects_an_inert_dso_before_activation_claim` — 0 pass, 2 fail
13. `hermit::cli$run_liteinst_verifies_detcore_backend` — 0 pass, 2 fail
14. `hermit::command_strict_verify$kernel_pseudofile_commands_are_deterministic_under_strict_verify` — 0 pass, 2 fail
15. `hermit::hermit_modes$verify_reports_exit_status_divergence` — 0 pass, 2 fail
16. `hermit::hermit_modes$verify_reports_stdout_divergence` — 0 pass, 2 fail
17. `hermit::hermit_modes$verify_verbose_compares_the_full_trace` — 0 pass, 2 fail
18. `hermit::liteinst_advanced$liteinst_fork_fails_closed_without_hanging` — 0 pass, 2 fail
19. `hermit::liteinst_advanced$liteinst_thread_clone_fails_closed_without_sigsys` — 0 pass, 2 fail
20. `hermit::sabre_examples$sabre_non_racy_examples_verify_current_envelope` — 0 pass, 1 fail
21. `hermit::signal_determinism$sigsuspend_without_signal_reports_terminal_deadlock` — 0 pass, 2 fail

## Current evidence and next action

Commits now present suggest fixes for the implicit-mount `/tmp` assertion, the seven DBT capture-name failures, the three LiteInst CLI failures caused by stale runtime staging, and the three stale `hermit_modes` exit assertions. These are not yet current-tree results.

First confirm this slot is clean and still at the recorded SHA, then run the cheapest focused check:

```bash
git status --short --branch
git rev-parse HEAD
cargo test -p hermit --features third-party-backends --bin hermit 'run::detects_symlink_resolution_through_implicit_mounts' -- --exact --nocapture
```

Before measuring DBT, Sabre, or LiteInst, stage exactly what validate uses:

```bash
HERMIT_INSTALL_FORCE_RESTAGE=hermit-125-classify ./ci/run-with-reverie-dbt-budget.sh cargo build --release --locked -p hermit --features third-party-backends -p detcore-dbt -p detcore-sabre -p hermit-install
```

Then run exact tests through the node environments recorded in `ci/dag/portable.json`. Do not treat backend-unavailable early returns as passes. Direct Hermit invocation outside the official test runner must use `bin/safehermit`.

Unverified: all current-tree outcomes and the final count in each owner category: understood infrastructure failure, understood prerequisite failure, genuine product failure, or no-result. The prior view that the two Go strict-verify tests and kernel pseudofile strict-verify test are product failures also needs current-tree measurement. Sabre and blocking `sigsuspend` remain unclassified.
