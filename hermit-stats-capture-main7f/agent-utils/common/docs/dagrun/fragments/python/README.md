## Install

```sh
python3 -m pip install dagrun
```

Python 3.10 or newer is required. The installation provides the `dagrun` and
`cpuset-alloc` console commands, an importable package of the same name, and
typing metadata.

## Python API

The model, serializer, planner, scheduler, and visualization helpers are public:

```python
from dagrun import DagConfig, Step, to_ascii

dag = DagConfig(steps=(Step("build", "app", "compile", "make build"),))
print(to_ascii(dag))
```

Use `dag_from_path(path)` to load a JSON/YAML file that may contain namespaced
`include` fragments. The context-free `dag_from_json` and `dag_from_yaml`
helpers deliberately refuse `include` because they have no safe base directory.

`run_dag(..., jobs=N)` keeps a compatibility combined setting: `N` bounds active
steps and caps each runner-controlled step's width unless `core_budget` is supplied. New code
should call `run_dag_limited(..., max_steps=S, max_cpus=P)` when those limits
differ. The former `cpu_jobs=P` keyword remains a compatibility alias. Library
calls do not establish an outer cgroup, so `max_cpus=P` is a whole-run bandwidth
cap only when the caller supplies equivalent outer containment; it is never a
summed-width admission gate.
