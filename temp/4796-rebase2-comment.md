Mechanical rebase of `drain/4796` onto main `0abad707bea52b77ef341b053fcbb3eca59769d1` (#4709 landed): new head `38251c3262527f57fb365a19290d6747523f34c0`.

- Range-diff `8b34630e..3eae8b4b545` vs `0abad707..38251c326`: every commit `=` except the DAG pin/seal pair (dropped/recreated; the `!` hunks are only `ci/dag/validate.json` and count pins in `validation_dag_static.rs`). No product semantic change; no assertion, tolerance, comparator or check was weakened.
- Config union keeps both #4709's `cpu_profile`/`rcb_clock_contract` and 4796's `watch`/`watch_from`/`hardware_watchpoints` (legacy keys 15, fields-without-legacy-key 54, enumerated in `detcore-model/src/config.rs`).
- RPC: revision stays 45 (main's last row 40; reserves 33-39 on main, 41-44 carried); digest `b434f4f465fdbb8d` re-measured at this head — `the_global_rpc_revision_matches_its_definitions` passes.
- Pins composed as main + (approved head - base): detcore_unit 3554 (+10), regular_crates 1960 (+2), hermit_unit 1276 (+1), cli 820 (+2), cli_on_host 739 (+2), twins likewise. `generate-validation-dag --check` and `generate-test-footprints --check` pass.
- Targeted tests at this head: detcore-model lib 348/348, hermit-detcore watch 9/9 + access_trace 41/41 + global_rpc 2/2, hermit-run watch 26/26 + run_config 25/25 + races 51/51 + access 5/5, CLI access_trace (ptrace) 13/13, validation_dag 46/46, fmt clean, clippy `-D warnings` clean for the four touched packages. Full manifest-plan lib has the same 6 pre-existing `ptrace_given_*` failures (need `HERMIT_BIN`) as on the unmodified tree.

Resubmitted to the merge queue at this head. Not landed.
