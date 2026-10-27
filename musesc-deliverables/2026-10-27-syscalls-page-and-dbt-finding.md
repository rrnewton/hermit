# Finding: Syscalls page not yet live; DBT histogram gap — 2026-10-27

Written by musesc (Muse) at the coordinator's request (2026-10-27 6:13 AM ET).

## Live Syscalls page: NOT live yet — no Docs rebuild has run since slice 3

Checked 2026-10-27 10:19–10:30 UTC (after the daily 08:23 UTC slot):

- `https://rrnewton.github.io/hermit/compatibility/latest/syscalls/index.html`
  returns **404** (fetched via `with-proxy curl`; body is the GitHub Pages
  "File not found" page, 9,379 bytes).
- The live index (`compatibility/latest/`, 438,308 bytes) has **no
  "Syscalls" navigation link** (cells/tests/runs only) and its data runs
  through 2026-10-25.
- GitHub API (via herdr-run): the newest Docs workflow run is
  [40347753399](https://github.com/rrnewton/hermit/actions/runs/40347753399)
  (2026-10-26T15:16:47Z, event `schedule`, conclusion **failure**). There is
  **no Docs run on 2026-10-27 at all** — today's 08:23 UTC scheduled run did
  not start.
- Slice 3 landed on dev-hermit main at `139ee26e` on 2026-10-26 18:10 -0700
  (2026-10-27 01:10 UTC), after that last failed run. So no rebuild has ever
  published with the Syscalls page, and the page cannot show counts until
  one does.

This is the exact failure shape the workflow file warns about
(`.github/workflows/docs.yml:11-16`: scheduled runs depend on the schedule
actor and on `vars.COMPATIBILITY_NIGHTLY_ENABLED`). The data side is ready
— published series rows at ledger `8e26a81f` carry run-1 histograms (see
`2026-10-27-syscall-coverage-gap.md`). **Ask:** dispatch Docs manually once
with `rebuild_compatibility`, then re-check this URL; if the scheduled run
keeps not starting, treat the gate variable/actor as the defect, not the
page.

## DBT 0 histogram rows: DBT runtime reporting gap (not a counting bug)

DBT shows 0 histogram rows because a DBT run produces no `--summary-json`
at all: its scheduler runs inside the DynamoRIO client and returns no run
summary to the launcher, so there is no `runtime` in DBT verify reports
and nothing for the series projection to carry. The other backends emit
the histogram from the launcher-side run summary.

Evidence in current Hermit main (slot `5f53234f`):

- `hermit-cli/tests/cli.rs:5385-5391` (in
  `dbt_runs_with_a_resolved_or_explicitly_disabled_maximum`'s helper):
  "The same per-run syscall histogram as on the other backends. DBT's
  launcher writes no summary JSON: its scheduler runs inside the
  DynamoRIO client and returns no run summary to the launcher." — followed
  by a tripwire asserting the summary file does **not** exist for DBT,
  and the checked histogram assertion for every other backend.
- October published rows quantify it: DBT 0/6,009 rows with a histogram
  vs ptrace 4,796/17,907 and kvm 4,516/11,234.

A DBT slice needs (handoff follow-up 2): a per-process sparse histogram in
the DBT stats record (`reverie-dbt/src/backend_stats.rs`, wire v2), a
launcher-side summary built from it, and `runtime` in DBT verify reports.
Until then DBT coverage on the Syscalls page will read "Without a
histogram" for every cell — that is accurate reporting of the gap, not a
website defect.
