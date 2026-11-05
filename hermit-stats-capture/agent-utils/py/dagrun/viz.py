"""Human-readable DAG visualizations: Graphviz DOT and quick ASCII art.

Pure functions over a :class:`~dagrun.model.DagConfig` — no I/O.

* :func:`to_dot` emits Graphviz (pipe to ``dot -Tsvg -o dag.svg``): one cluster per group,
  solid dependency edges, and a dashed edge chain for each cap-1 scarce resource (a visual
  hint that those steps serialize).
* :func:`to_ascii` emits a compact topological-layer view for a glance in the terminal —
  each step on its layer (longest dependency depth), with its class, scarce-resource demand,
  and immediate dependencies.
* :func:`to_dot_groups` / :func:`to_ascii_groups` collapse the steps to one node per ``group``,
  with step counts and merged cross-group dependency-edge counts.
"""

from __future__ import annotations

from dagrun.model import DagConfig, Step, step_classification

__all__ = ["GROUP_BY_KEYS", "to_ascii", "to_ascii_groups", "to_dot", "to_dot_groups"]

#: The one ``--group-by`` key the renderers understand: a step's ``group`` field.
GROUP_BY_KEYS: tuple[str, ...] = ("group",)


def _selected_steps(cfg: DagConfig, selected: set[str] | None) -> list[Step]:
    return [s for s in cfg.steps if selected is None or s.tag in selected]


def _kept_deps(steps: list[Step]) -> dict[str, list[str]]:
    """Each step's deps, filtered to steps that are actually present in the selection."""
    tags = {s.tag for s in steps}
    return {s.tag: [d for d in s.deps if d in tags] for s in steps}


def _profile_suffix(step: Step) -> str:
    r"""Concise per-node profiling annotation for a DOT label: ``\n{est}s, {mb}MB``
    (expected wall-seconds and max resident memory in decimal MB, floored).

    Returns ``""`` when the step carries neither a duration nor an RSS estimate, so
    undecorated DAGs render exactly as before. Formatting is integer-floored for MB and
    fixed one-decimal for seconds so the Python and Rust builds stay byte-identical.
    """
    est = step.hint.est_duration_s
    rss = step.hint.rss_baseline_bytes
    if est <= 0.0 and rss is None:
        return ""
    mb = (rss or 0) // 1_000_000
    return f"\\n{est:.1f}s, {mb}MB"


def _critical_path_seconds(steps: list[Step], deps: dict[str, list[str]]) -> float:
    """Longest weighted finish time over the DAG (critical path in expected seconds),
    weighting each node by its ``est_duration_s``. Memoized; visits in ``steps`` order."""
    est_of = {s.tag: s.hint.est_duration_s for s in steps}
    finish: dict[str, float] = {}

    def visit(tag: str) -> float:
        if tag in finish:
            return finish[tag]
        parents = deps.get(tag, [])
        base = max((visit(p) for p in parents), default=0.0)
        finish[tag] = base + est_of.get(tag, 0.0)
        return finish[tag]

    return max((visit(s.tag) for s in steps), default=0.0)


def _scaling_suffix(steps: list[Step], deps: dict[str, list[str]]) -> str:
    """Graph-title scaling annotation: ``  |  {N.N}X max par-spdup``, the ideal parallel
    speedup (total serial work / critical path). Omitted when no step has a duration estimate
    (critical path is zero), so undecorated DAGs render exactly as before."""
    serial = sum(s.hint.est_duration_s for s in steps)
    crit = _critical_path_seconds(steps, deps)
    if crit <= 0.0:
        return ""
    return f"  |  {serial / crit:.1f}X max par-spdup"


def to_dot(cfg: DagConfig, *, name: str = "dag", selected: set[str] | None = None) -> str:
    """Render the DAG as Graphviz DOT."""
    steps = _selected_steps(cfg, selected)
    deps = _kept_deps(steps)
    by_group: dict[str, list[Step]] = {}
    for step in steps:
        by_group.setdefault(step.group, []).append(step)

    scaling = _scaling_suffix(steps, deps)
    out: list[str] = [
        f"digraph {name} {{",
        "  rankdir=LR;",
        "  node [shape=box, style=rounded, fontsize=10];",
        '  labelloc="t";',
        f'  label="DAG  (solid = dependency;  dashed = shared cap-1 resource -> serialized){scaling}";',
    ]
    for i, group in enumerate(sorted(by_group)):
        out.append(f"  subgraph cluster_{i} {{")
        out.append(f'    label="{group}"; style=dashed; color=gray70;')
        for step in sorted(by_group[group], key=lambda s: s.job):
            suffix = _profile_suffix(step)
            out.append(
                f'    "{step.tag}" [label="{step.tag}\\n[{step_classification(step).value}]{suffix}"];'
            )
        out.append("  }")

    for step in steps:
        for dep in deps[step.tag]:
            out.append(f'  "{dep}" -> "{step.tag}";')

    # A dashed chain across the users of each cap-1 resource: a hint that they serialize.
    for res, cap in sorted(cfg.resource_caps.items()):
        if cap == 1:
            users = sorted(s.tag for s in steps if s.hint.resources.get(res, 0) > 0)
            for left, right in zip(users, users[1:]):
                out.append(
                    f'  "{left}" -> "{right}" '
                    f'[style=dashed, color=gray60, constraint=false, label="{res}"];'
                )
    out.append("}")
    return "\n".join(out) + "\n"


def _layers(steps: list[Step], deps: dict[str, list[str]]) -> dict[str, int]:
    """Longest-dependency-depth layer for each step tag."""
    depth: dict[str, int] = {}

    def visit(tag: str) -> int:
        if tag in depth:
            return depth[tag]
        parents = deps.get(tag, [])
        depth[tag] = 0 if not parents else 1 + max(visit(p) for p in parents)
        return depth[tag]

    for step in steps:
        visit(step.tag)
    return depth


def to_ascii(cfg: DagConfig, *, selected: set[str] | None = None) -> str:
    """Render the DAG as compact ASCII art, grouped by topological layer."""
    steps = _selected_steps(cfg, selected)
    deps = _kept_deps(steps)
    depth = _layers(steps, deps)
    layers: dict[int, list[Step]] = {}
    for step in steps:
        layers.setdefault(depth[step.tag], []).append(step)

    edge_count = sum(len(d) for d in deps.values())
    width = max((len(s.tag) for s in steps), default=1)
    out: list[str] = [
        f"DAG - {len(steps)} steps, {edge_count} edges, {len(layers)} layer(s)",
        "",
    ]
    for level in sorted(layers):
        out.append(f"layer {level}:")
        for step in sorted(layers[level], key=lambda s: s.tag):
            res = "".join(f" {{{k}:{v}}}" for k, v in sorted(step.hint.resources.items()))
            dep = "  <- " + ", ".join(sorted(deps[step.tag])) if deps[step.tag] else ""
            out.append(f"  {step.tag:<{width}}  [{step_classification(step).value}]{res}{dep}")
    return "\n".join(out) + "\n"


def _dot_quote(s: str) -> str:
    """Escape a name for a double-quoted DOT ID or label (group-level renderer only)."""
    return s.replace("\\", "\\\\").replace('"', '\\"')


def _step_count_text(n: int) -> str:
    return "1 step" if n == 1 else f"{n} steps"


def _group_graph(
    steps: list[Step], deps: dict[str, list[str]]
) -> tuple[dict[str, int], dict[tuple[str, str], int], int, int]:
    """The step graph collapsed onto its groups.

    Returns (step count per group, step-edge count per ordered (dependency group -> dependent
    group) pair, cross-group step edges, within-group step edges). Edges inside one group are
    counted separately and never become a group edge."""
    group_of = {s.tag: s.group for s in steps}
    counts: dict[str, int] = {}
    edges: dict[tuple[str, str], int] = {}
    cross = intra = 0
    for step in steps:
        counts[step.group] = counts.get(step.group, 0) + 1
        for dep in deps[step.tag]:
            src = group_of[dep]
            if src == step.group:
                intra += 1
            else:
                cross += 1
                edges[(src, step.group)] = edges.get((src, step.group), 0) + 1
    return counts, edges, cross, intra


def to_dot_groups(cfg: DagConfig, *, name: str = "dag", selected: set[str] | None = None) -> str:
    """Render the DAG collapsed to one node per ``group``, as Graphviz DOT.

    Each node is labelled with its group name and step count. Each edge is one ordered
    (dependency group -> dependent group) pair, labelled with the number of step-level
    dependency edges it merges; dependencies inside one group draw no edge."""
    steps = _selected_steps(cfg, selected)
    deps = _kept_deps(steps)
    counts, edges, _cross, _intra = _group_graph(steps, deps)
    scaling = _scaling_suffix(steps, deps)
    out: list[str] = [
        f"digraph {name} {{",
        "  rankdir=LR;",
        "  node [shape=box, style=rounded, fontsize=10];",
        '  labelloc="t";',
        "  label=\"DAG by group  (node = group and its step count;  "
        f'edge label = step-level dependencies merged){scaling}";',
    ]
    for group in sorted(counts):
        q = _dot_quote(group)
        out.append(f'  "{q}" [label="{q}\\n{_step_count_text(counts[group])}"];')
    for (src, dst), n in sorted(edges.items()):
        out.append(f'  "{_dot_quote(src)}" -> "{_dot_quote(dst)}" [label="{n}"];')
    out.append("}")
    return "\n".join(out) + "\n"


def to_ascii_groups(cfg: DagConfig, *, selected: set[str] | None = None) -> str:
    """Render the DAG collapsed to one line per ``group``, as compact ASCII.

    Groups are ordered by the earliest dependency layer any of their steps occupies, then by
    name. Each line shows the step count and the upstream groups with their merged edge counts."""
    steps = _selected_steps(cfg, selected)
    deps = _kept_deps(steps)
    depth = _layers(steps, deps)
    counts, edges, cross, intra = _group_graph(steps, deps)
    first_layer: dict[str, int] = {}
    for step in steps:
        d = depth[step.tag]
        first_layer[step.group] = min(d, first_layer.get(step.group, d))
    upstream: dict[str, list[tuple[str, int]]] = {}
    for (src, dst), n in sorted(edges.items()):
        upstream.setdefault(dst, []).append((src, n))
    order = sorted(first_layer, key=lambda g: (first_layer[g], g))
    width = max((len(g) for g in counts), default=1)
    count_width = max((len(_step_count_text(n)) for n in counts.values()), default=1)
    out: list[str] = [
        f"DAG by group - {len(counts)} groups, {len(steps)} steps, {len(edges)} group edges "
        f"({cross} cross-group step edges merged; {intra} within groups)",
        "",
    ]
    for group in order:
        count = _step_count_text(counts[group])
        ups = upstream.get(group)
        if ups:
            joined = ", ".join(f"{u} ({n})" for u, n in ups)
            out.append(f"  {group:<{width}}  {count:<{count_width}}  <- {joined}")
        else:
            out.append(f"  {group:<{width}}  {count}")
    return "\n".join(out) + "\n"
