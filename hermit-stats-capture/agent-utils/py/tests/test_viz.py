"""Tests for dagrun.viz (synthetic DAG)."""

from __future__ import annotations

from dagrun.model import DagConfig, ResourceHint, Step
from dagrun.viz import to_ascii, to_dot


def _cfg() -> DagConfig:
    return DagConfig(
        steps=(
            Step("build", "app", "", "true"),
            Step("test", "unit", "", "true", deps=["build.app"]),
            Step("e2e", "a", "", "true", deps=["build.app"], hint=ResourceHint(resources={"browser": 1})),
            Step("e2e", "b", "", "true", deps=["build.app"], hint=ResourceHint(resources={"browser": 1})),
        ),
        resource_caps={"browser": 1},
    )


def test_dot_has_clusters_nodes_and_edges() -> None:
    dot = to_dot(_cfg())
    assert dot.startswith("digraph dag {")
    assert '"build.app" -> "test.unit";' in dot
    assert '"build.app" -> "e2e.a";' in dot
    # cap-1 browser resource -> a dashed serialization edge between the two browser steps
    assert '"e2e.a" -> "e2e.b" [style=dashed' in dot
    assert dot.rstrip().endswith("}")


def test_ascii_shows_layers_deps_and_resources() -> None:
    art = to_ascii(_cfg())
    assert "layer 0:" in art and "layer 1:" in art
    assert "build.app" in art
    assert "<- build.app" in art  # dependent lists its dep
    assert "{browser:1}" in art  # resource demand shown


def test_dot_omits_profiling_when_no_estimates() -> None:
    # An undecorated DAG (no est/rss) renders exactly as before: no "Xs, YMB" and no scaling.
    dot = to_dot(_cfg())
    assert '"build.app" [label="build.app\\n[light]"];' in dot
    assert "max par-spdup" not in dot
    assert "MB" not in dot


def _profiled_cfg() -> DagConfig:
    # a: 30s -> b: 60s = 90s critical path; off-path c: 30s. Serial 120s, ideal speedup 1.3X.
    return DagConfig(
        steps=(
            Step("build", "a", "", "true", hint=ResourceHint(est_duration_s=30.0, rss_baseline_bytes=268_435_456)),
            Step(
                "test",
                "b",
                "",
                "true",
                deps=["build.a"],
                hint=ResourceHint(est_duration_s=60.0, rss_baseline_bytes=3_221_225_472),
            ),
            Step(
                "test",
                "c",
                "",
                "true",
                deps=["build.a"],
                hint=ResourceHint(est_duration_s=30.0, rss_baseline_bytes=1_073_741_824),
            ),
        ),
    )


def test_dot_annotates_profiling_and_scaling() -> None:
    dot = to_dot(_profiled_cfg())
    # Per-node "est-s, RSS-MB" (RSS floored to decimal MB).
    assert '"build.a" [label="build.a\\n[light]\\n30.0s, 268MB"];' in dot
    assert '"test.b" [label="test.b\\n[light]\\n60.0s, 3221MB"];' in dot
    # Graph-title scaling: serial 120 / critpath 90 = 1.3X.
    assert "|  1.3X max par-spdup" in dot


# --------------------------------------------------------------------------- --labels / --group-by
# The same three-group fixture and exact bytes as rs/dagrun/tests/viz_groups_cli.rs, so the two
# editions are pinned to identical output.
_GROUPS_DAG = """{"steps":[
  {"group":"build","job":"app","cmd":"true","labels":["full"]},
  {"group":"build","job":"lib","cmd":"true","deps":["build.app"]},
  {"group":"test","job":"unit","cmd":"true","labels":["quick","full"],"deps":["build.app","build.lib"]},
  {"group":"test","job":"lint","cmd":"true","labels":["quick"],"deps":["build.app"]},
  {"group":"e2e","job":"smoke","cmd":"true","labels":["full"],"deps":["build.app","test.unit"]}
]}
"""

_GROUPS_DEFAULT_DOT = r"""digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG  (solid = dependency;  dashed = shared cap-1 resource -> serialized)";
  subgraph cluster_0 {
    label="build"; style=dashed; color=gray70;
    "build.app" [label="build.app\n[light]"];
    "build.lib" [label="build.lib\n[light]"];
  }
  subgraph cluster_1 {
    label="e2e"; style=dashed; color=gray70;
    "e2e.smoke" [label="e2e.smoke\n[light]"];
  }
  subgraph cluster_2 {
    label="test"; style=dashed; color=gray70;
    "test.lint" [label="test.lint\n[light]"];
    "test.unit" [label="test.unit\n[light]"];
  }
  "build.app" -> "build.lib";
  "build.app" -> "test.unit";
  "build.lib" -> "test.unit";
  "build.app" -> "test.lint";
  "build.app" -> "e2e.smoke";
  "test.unit" -> "e2e.smoke";
}
"""

_GROUPS_ALL_DOT = r"""digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG by group  (node = group and its step count;  edge label = step-level dependencies merged)";
  "build" [label="build\n2 steps"];
  "e2e" [label="e2e\n1 step"];
  "test" [label="test\n2 steps"];
  "build" -> "e2e" [label="1"];
  "build" -> "test" [label="3"];
  "test" -> "e2e" [label="1"];
}
"""

_GROUPS_FULL_DOT = r"""digraph dag {
  rankdir=LR;
  node [shape=box, style=rounded, fontsize=10];
  labelloc="t";
  label="DAG by group  (node = group and its step count;  edge label = step-level dependencies merged)";
  "build" [label="build\n2 steps"];
  "e2e" [label="e2e\n1 step"];
  "test" [label="test\n1 step"];
  "build" -> "e2e" [label="1"];
  "build" -> "test" [label="2"];
  "test" -> "e2e" [label="1"];
}
"""


def _view(args: list[str]) -> tuple[int, str, str]:
    import contextlib
    import io
    import tempfile
    from pathlib import Path

    from dagrun.cli import main

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(_GROUPS_DAG, encoding="utf-8")
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                rc = main([*args, "--dag", str(path)])
            except SystemExit as exc:  # argparse refusals
                rc = int(exc.code or 0)
        return rc, out.getvalue(), err.getvalue()


def test_default_dot_is_unchanged_on_group_fixture() -> None:
    assert _view(["dot"]) == (0, _GROUPS_DEFAULT_DOT, "")


def test_group_dot_counts_steps_and_merged_cross_group_edges() -> None:
    assert _view(["dot", "--group-by", "group"]) == (0, _GROUPS_ALL_DOT, "")
    assert _view(["dot", "--group-by=group"]) == (0, _GROUPS_ALL_DOT, "")


def test_label_filter_keeps_labelled_steps_and_their_dependencies() -> None:
    assert _view(["dot", "--labels", "full", "--group-by", "group"]) == (0, _GROUPS_FULL_DOT, "")
    rc, out, _ = _view(["dot", "--labels=quick"])
    assert rc == 0
    assert '"e2e.smoke"' not in out and '"build.lib" -> "test.unit";' in out
    assert _view(["dot", "--labels", "quick,full"]) == (0, _GROUPS_DEFAULT_DOT, "")


def test_group_ascii_lists_counts_and_upstream_groups() -> None:
    assert _view(["ascii", "--labels", "full", "--group-by", "group"]) == (
        0,
        "DAG by group - 3 groups, 4 steps, 3 group edges "
        "(4 cross-group step edges merged; 1 within groups)\n"
        "\n"
        "  build  2 steps\n"
        "  test   1 step   <- build (2)\n"
        "  e2e    1 step   <- build (1), test (1)\n",
        "",
    )


def test_bad_view_flags_exit_2() -> None:
    assert _view(["dot", "--labels", "nope"]) == (
        2,
        "",
        "dagrun: --labels: unknown label(s): nope. Known labels: full, quick\n",
    )
    assert _view(["ascii", "--labels", ","]) == (
        2,
        "",
        "dagrun ascii: error: --labels requires at least one label\n",
    )
    assert _view(["dot", "--group-by", "job"])[0] == 2
    assert _view(["dot", "--labels", "-x", "--group-by", "group"])[0] == 2
    assert _view(["list", "--group-by", "group"])[0] == 2


def test_group_dot_quotes_names_and_draws_no_self_edges() -> None:
    from dagrun.viz import to_ascii_groups, to_dot_groups

    cfg = DagConfig(
        steps=(
            Step('we"ird\\grp', "a", "", "true"),
            Step('we"ird\\grp', "b", "", "true", deps=['we"ird\\grp.a']),
        ),
    )
    dot = to_dot_groups(cfg)
    assert '  "we\\"ird\\\\grp" [label="we\\"ird\\\\grp\\n2 steps"];\n' in dot
    assert "->" not in dot
    assert to_ascii_groups(cfg).startswith(
        "DAG by group - 1 groups, 2 steps, 0 group edges "
        "(0 cross-group step edges merged; 1 within groups)\n"
    )
