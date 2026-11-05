# Repository checks and documentation tooling

These scripts enforce contracts that span otherwise independent packages:

They are repository workflow, evidence, and acquisition helpers rather than commands in the
independently installable paired-tool inventory. The agent-log archive fetcher is acquisition
support for the explicitly Python-only timeline/trace family and is intentionally not exposed
through the top-level `bin/` command surface while that family is still evolving.

| Script | Purpose |
| --- | --- |
| `embed_userguides.py` | Render shared documentation and verify package links and standalone prose. |
| `check_deps.py` | Smoke every command in a minimal environment to catch import-time dependency failures. |
| `check_no_any.py` | Reject explicit `typing.Any` use at typed boundaries. |
| `check_python_packages.py` | Build each wheel and sdist, rebuild from the sdist, then inspect, install, and smoke every Python distribution in isolation. |
| `check_rust_packages.py` | Package and inspect each Rust crate in isolation. |
| `check_site.py` | Check the public website under `site/`: pages parse, every relative and repository link resolves (including `#fragments` into Markdown headings), nothing loads from another origin, every published file is tracked by git, and asset sizes stay within their caps. `--stage DIR` copies the published files (all but `site/demos/` and `site/README.md`) for the Pages workflow. |
| `dagrun_profile_chart.py` | Render deterministic, dependency-free SVG speedup and within-step parallelism charts from dagrun model/trace data. |
| `agent-log-archive/fetch_agent_logs.py` | Strict-typed, non-deleting local/SSH archive fetcher with manifests, metrics, and immutable receipts (`.sh` is its compatibility launcher). |
| `irq_survey.py` | Re-derive the per-CPU interrupt numbers the core allocator's ranking rests on: is there a signal, does it change placement, and how far does an independent window drift. |
| `rebase-delta-guard` | Bracket a rebase and prove its complete text and binary delta is unchanged. `record` captures the exact intended onto SHA; recorded series must be linear and descend from their base, while `check` accepts four explicit snapshot revisions. During a stopped rebase, use `conflict-files` instead of raw `git diff --name-only --diff-filter=U`: it reads index modes and omits conflicted submodule gitlinks before file-oriented tools can recurse into them. |

Run the focused checks directly:

```sh
python3 scripts/embed_userguides.py --check
python3 scripts/check_deps.py
python3 scripts/check_no_any.py .
python3 scripts/check_site.py --self-test && python3 scripts/check_site.py
python3 scripts/irq_survey.py distribution
make check-packages
```

Render one step's fitted speedup curve from a scaling-model sidecar, or its achieved parallelism
over time from an opt-in trace:

```sh
mkdir -p /tmp/dagrun-charts
python3 scripts/dagrun_profile_chart.py speedup \
  --model .dagrun/profiles/scaling_model_MACHINE_CONTAINER.json \
  --step build.app \
  --output /tmp/dagrun-charts/build-speedup.svg \
  --csv-out /tmp/dagrun-charts/build-speedup.csv
python3 scripts/dagrun_profile_chart.py timeline \
  --trace .dagrun/profiles/traces/RUN_ID.csv \
  --step build.app \
  --output /tmp/dagrun-charts/build-timeline.svg \
  --csv-out /tmp/dagrun-charts/build-timeline.csv
```

The renderer validates the expected model or trace schema before selecting rows. It does not create
missing output directories. The optional CSV is the exact compact series supplied to the SVG, which
makes the chart reproducible and easy to inspect without an image viewer.

Publication is ordinary git — `git pull --rebase origin main && git push origin
main`. `main` is protected server-side by a GitHub ruleset (no deletion, no
force-push, no merge commits); see `AGENTS.md`.

Use `make check`, `make test`, and `make cross` for the wider repository contract.
