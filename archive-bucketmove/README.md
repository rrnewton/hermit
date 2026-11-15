Bucket move for D132130650 (preempt-spin-flag: bin-c -> c-programs),
prepared 2026-11-15 against the rebased V474436399 tree (fbsource
master c179845d + the seat-approved V0.4 cell). bucket-move.patch is
the sl diff from the rebased v4 commit to the bucket-move commit
(fbsource fbcode/hermetic_infra/hermit paths). rename-table.json is
the 19-row 1:1 scorecard rename proof (17822 rows both sides,
payload-identical). Apply as the follow-up diff after D132130650
lands; regenerate with ci/sync-cell-config.sh at the landing base.
