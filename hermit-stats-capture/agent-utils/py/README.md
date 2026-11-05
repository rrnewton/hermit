# agent-utils for Python

This directory contains the independently installable Python editions of the
agent-utils command-line tools. Each distribution owns one import package, its
own version and documentation, and only the console commands that belong to it.

| Distribution | Import package | Commands | Purpose |
| --- | --- | --- | --- |
| `dagrun` | `dagrun` | `dagrun`, `cpuset-alloc` | Run and inspect resource-aware CI DAGs; the companion allocator reserves isolated CPU sets for benchmarks. |
| `tick-hub` | `tick_hub` | `tick-hub` | Evaluate cadenced reminders and health checks in one deterministic tick. |
| `pr-landing-planner` | `pr_landing_planner` | `pr-landing-planner` | Produce an advisory, conflict-aware pull-request landing plan. |
| `wrkviz` | `wrkviz` | `wrkviz` | Build a durable, zoomable local timeline from coordinator and subagent transcripts. |
| `parallel-experiment-runner` | `parallel_experiment_runner` | `parallel-experiment-runner` | Run boxed, resource-bounded concurrent seed sweeps. |
| `herdr-run` | `herdr_run` | `herdr-run` | Execute policy-admitted shell commands through Herdr panes. |
| `agentctl` | `agentctl` | `agentctl` (plus compatibility aliases) | Control persistent interactive and headless agents; optional Chat and MCP access. |
| `wrkslots` | `wrkslots` | `wrkslots` | Create, track, hand off, recover, and conservatively remove coding-agent Git worktree slots. |

Install a tool from its project directory during development:

```sh
python3 -m pip install ./py/dagrun
python3 -m pip install ./py/tick_hub
python3 -m pip install ./py/pr_landing_planner
python3 -m pip install ./py/wrkviz
python3 -m pip install ./py/parallel_experiment_runner
python3 -m pip install ./py/herdr_run
python3 -m pip install ./py/agentctl
python3 -m pip install ./py/wrkslots
```

The root `pyproject.toml` contains shared type-checker configuration only; it is
deliberately not a publishable aggregate distribution. Repository development
continues to use the flat source tree:

```sh
cd py
python3 -m mypy .
python3 -m pytest -q
```

Run `make check-python-packages` from the repository root to build each wheel and
source distribution in isolation, rebuild the wheel from the source archive,
inspect its contents, install it without dependency or network access, and
smoke all of its declared commands. The invoking interpreter must
already provide `setuptools>=77`; the offline check never downloads its build
backend.
