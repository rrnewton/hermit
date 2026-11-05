## Install

```sh
python3 -m pip install pr-landing-planner
```

Python 3.10 or newer is required. Installation provides the
`pr-landing-planner` console command, the typed `pr_landing_planner` package,
and deterministic example fixtures.

## Python API

The pure planning core and fixture host are importable:

```python
from pr_landing_planner import FakeHost, assemble_result, collect_graph
from pr_landing_planner.fakehost import load_fixture_text

fixture = load_fixture_text('{"repo":"owner/repo","prs":[]}', as_yaml=False)
host, repository, base = FakeHost.from_fixture(fixture)
result = assemble_result(collect_graph(host, repo=repository, base=base))
```
