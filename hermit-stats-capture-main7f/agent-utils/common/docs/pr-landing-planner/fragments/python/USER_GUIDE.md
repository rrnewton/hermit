## Installation and library use

```sh
python3 -m pip install pr-landing-planner
```

Python 3.10 or newer is required. Installation provides the console command,
the typed `pr_landing_planner` package, fixtures, and this guide as package
data. Live collection also requires `gh` and `git` on `PATH`.

```python
from pr_landing_planner import FakeHost, assemble_result, collect_graph, render_json
from pr_landing_planner.fakehost import load_fixture_text

fixture = load_fixture_text('{"repo":"owner/repo","prs":[]}', as_yaml=False)
host, repository, base = FakeHost.from_fixture(fixture)
graph = collect_graph(host, repo=repository, base=base)
print(render_json(assemble_result(graph)))
```

The public API includes typed models, CI classification, graph construction,
priority providers, deterministic renderers, and the `VcsHost` protocol for
custom data sources.
