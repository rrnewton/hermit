"""Path-aware DAG composition is strict, confined, and namespace-safe."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from dagrun.io import (
    MAX_DAG_FLATTENED_STEPS,
    MAX_DAG_INCLUDE_DEPTH,
    MAX_DAG_INCLUDE_INSTANCES,
    DagJsonError,
    dag_from_json,
    dag_from_path,
    dag_from_yaml,
    dag_to_json,
)
from dagrun.model import StructuredTestResultsManifest
from dagrun.profile_report import load_dag_config


def _write(path: Path, value: object) -> None:
    path.write_text(json.dumps(value), encoding="utf-8")


def test_context_free_parsers_reserve_include_instead_of_ignoring_it() -> None:
    for load, document in (
        (dag_from_json, '{"include": [], "steps": []}'),
        (dag_from_yaml, "include: []\nsteps: []\n"),
    ):
        with pytest.raises(DagJsonError, match="includes require filesystem context"):
            load(document)

    with pytest.raises(DagJsonError, match="include: must be a list"):
        dag_from_json('{"include": {}, "steps": []}')
    with pytest.raises(DagJsonError, match="expected an object, got number"):
        dag_from_json('{"include": [1], "steps": []}')
    with pytest.raises(DagJsonError, match=r"unknown field\(s\) 'future'"):
        dag_from_json(
            '{"include":[{"path":"child.json","namespace":"child","future":1}],'
            '"steps":[]}'
        )
    with pytest.raises(DagJsonError, match="after: must be a list of strings"):
        dag_from_json(
            '{"include":[{"path":"child.json","namespace":"child","after":"g.j"}],'
            '"steps":[]}'
        )


def test_nested_json_yaml_includes_flatten_and_rewrite_identities(tmp_path: Path) -> None:
    _write(
        tmp_path / "leaf.json",
        {
            "resource_caps": {"browser": 1},
            "steps": [
                {
                    "group": "build",
                    "job": "app",
                    "cmd": "true",
                    "deps": None,
                    "labels": ["fast"],
                    "fail_fast_family": "compile",
                    "result_manifests": [
                        {
                            "kind": "structured-test-results",
                            "schema": 2,
                            "path_env": "DAGRUN_TEST_COUNTS_PATH",
                            "owner": "build.app",
                        }
                    ],
                }
            ],
        },
    )
    (tmp_path / "middle.yaml").write_text(
        """include:
  - path: leaf.json
    namespace: leaf
    after: [prep.ready]
description: child description is not promoted
resource_caps:
  gpu: 2
steps:
  - group: test
    job: unit
    cmd: "true"
    deps: [leaf.build.app]
  - group: diagnose
    job: unit
    cmd: "true"
    explains: [test.unit]
  - group: prep
    job: ready
    cmd: "true"
""",
        encoding="utf-8",
    )
    _write(
        tmp_path / "root.json",
        {
            "description": "root description",
            "include": [
                {
                    "path": "middle.yaml",
                    "namespace": "component",
                    "after": ["setup.ready"],
                }
            ],
            "resource_caps": {"browser": 1},
            "steps": [
                {"group": "setup", "job": "ready", "cmd": "true"},
                {
                    "group": "publish",
                    "job": "all",
                    "cmd": "true",
                    "deps": ["component.test.unit"],
                }
            ],
        },
    )

    cfg = dag_from_path(tmp_path / "root.json")
    assert [step.tag for step in cfg.steps] == [
        "component.leaf.build.app",
        "component.test.unit",
        "component.diagnose.unit",
        "component.prep.ready",
        "setup.ready",
        "publish.all",
    ]
    assert cfg.steps[0].deps == ["component.prep.ready"]
    assert cfg.steps[1].deps == ["component.leaf.build.app"]
    assert cfg.steps[2].deps == ["setup.ready"]
    assert cfg.steps[2].explains == ["component.test.unit"]
    assert cfg.steps[3].deps == ["setup.ready"]
    # An outer dependency may target one exact internal node.  Includes do not synthesize a
    # completion node or turn that edge into an implicit wait for every terminal in the fragment.
    assert cfg.steps[5].deps == ["component.test.unit"]
    assert cfg.steps[0].fail_fast_family == "component.leaf.compile"
    assert cfg.steps[0].labels == ["fast"]
    assert cfg.steps[0].structured_test_results_manifest() == (
        StructuredTestResultsManifest.current("component.leaf.build.app")
    )
    assert cfg.resource_caps == {"browser": 1, "gpu": 2}
    assert cfg.description == "root description"
    assert "include" not in json.loads(dag_to_json(cfg))
    assert [step.tag for step in load_dag_config(tmp_path / "root.json").steps] == [
        step.tag for step in cfg.steps
    ]


def test_include_after_wires_every_upstream_to_every_internal_root_only(
    tmp_path: Path,
) -> None:
    _write(
        tmp_path / "child.json",
        {
            "steps": [
                {"group": "g", "job": "first", "cmd": "true"},
                {"group": "g", "job": "second", "cmd": "true", "deps": None},
                {
                    "group": "g",
                    "job": "descendant",
                    "cmd": "true",
                    "deps": ["g.first"],
                },
            ]
        },
    )
    _write(
        tmp_path / "root.json",
        {
            "include": [
                {
                    "path": "child.json",
                    "namespace": "child",
                    "after": ["setup.one", "setup.two"],
                }
            ],
            "steps": [
                {"group": "setup", "job": "one", "cmd": "true"},
                {"group": "setup", "job": "two", "cmd": "true"},
                {
                    "group": "publish",
                    "job": "exact",
                    "cmd": "true",
                    "deps": ["child.g.first"],
                },
            ],
        },
    )

    cfg = dag_from_path(tmp_path / "root.json")
    by_tag = {step.tag: step for step in cfg.steps}
    assert set(by_tag) == {
        "child.g.first",
        "child.g.second",
        "child.g.descendant",
        "setup.one",
        "setup.two",
        "publish.exact",
    }
    assert by_tag["child.g.first"].deps == ["setup.one", "setup.two"]
    assert by_tag["child.g.second"].deps == ["setup.one", "setup.two"]
    assert by_tag["child.g.descendant"].deps == ["child.g.first"]
    assert by_tag["publish.exact"].deps == ["child.g.first"]


def test_sibling_after_reference_is_independent_of_include_order(tmp_path: Path) -> None:
    _write(tmp_path / "a.json", {"steps": [{"group": "g", "job": "ready", "cmd": "true"}]})
    _write(tmp_path / "b.json", {"steps": [{"group": "g", "job": "start", "cmd": "true"}]})

    for name, includes in (
        (
            "forward.json",
            [
                {"path": "b.json", "namespace": "b", "after": ["a.g.ready"]},
                {"path": "a.json", "namespace": "a"},
            ],
        ),
        (
            "reverse.json",
            [
                {"path": "a.json", "namespace": "a"},
                {"path": "b.json", "namespace": "b", "after": ["a.g.ready"]},
            ],
        ),
    ):
        _write(tmp_path / name, {"include": includes, "steps": []})
        by_tag = {step.tag: step for step in dag_from_path(tmp_path / name).steps}
        assert by_tag["b.g.start"].deps == ["a.g.ready"]


def test_empty_include_with_valid_after_is_a_noop(tmp_path: Path) -> None:
    _write(tmp_path / "empty.json", {"steps": []})
    _write(
        tmp_path / "root.json",
        {
            "include": [
                {"path": "empty.json", "namespace": "empty", "after": ["setup.ready"]}
            ],
            "steps": [{"group": "setup", "job": "ready", "cmd": "true"}],
        },
    )

    assert [step.tag for step in dag_from_path(tmp_path / "root.json").steps] == [
        "setup.ready"
    ]


def test_outer_dependency_typo_for_included_step_is_an_ordinary_missing_tag(
    tmp_path: Path,
) -> None:
    _write(tmp_path / "child.json", {"steps": [{"group": "g", "job": "real", "cmd": "true"}]})
    _write(
        tmp_path / "root.json",
        {
            "include": [{"path": "child.json", "namespace": "child"}],
            "steps": [
                {
                    "group": "outer",
                    "job": "consumer",
                    "cmd": "true",
                    "deps": ["child.g.typo"],
                }
            ],
        },
    )

    with pytest.raises(
        DagJsonError,
        match=r"depends on 'child\.g\.typo', which no step declares",
    ):
        dag_from_path(tmp_path / "root.json")


def test_same_file_may_be_reused_only_under_different_namespaces(tmp_path: Path) -> None:
    _write(tmp_path / "child.json", {"steps": [{"group": "g", "job": "j", "cmd": "true"}]})
    _write(
        tmp_path / "good.json",
        {
            "include": [
                {"path": "child.json", "namespace": "one"},
                {"path": "child.json", "namespace": "two"},
            ],
            "steps": [],
        },
    )
    assert [step.tag for step in dag_from_path(tmp_path / "good.json").steps] == [
        "one.g.j",
        "two.g.j",
    ]

    _write(
        tmp_path / "bad.json",
        {
            "include": [
                {"path": "child.json", "namespace": "same"},
                {"path": "child.json", "namespace": "same"},
            ],
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError, match="duplicate include"):
        dag_from_path(tmp_path / "bad.json")

    (tmp_path / "child-alias.json").symlink_to(tmp_path / "child.json")
    _write(
        tmp_path / "alias-bad.json",
        {
            "include": [
                {"path": "child.json", "namespace": "same"},
                {"path": "child-alias.json", "namespace": "same"},
            ],
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError, match="duplicate include"):
        dag_from_path(tmp_path / "alias-bad.json")


def test_include_cycle_reports_canonical_chain(tmp_path: Path) -> None:
    _write(
        tmp_path / "a.json",
        {"include": [{"path": "b.json", "namespace": "b"}], "steps": []},
    )
    _write(
        tmp_path / "b.json",
        {"include": [{"path": "a.json", "namespace": "a"}], "steps": []},
    )
    with pytest.raises(DagJsonError, match=r"include cycle: .*a\.json.*b\.json.*a\.json"):
        dag_from_path(tmp_path / "a.json")


@pytest.mark.parametrize(
    "included",
    [
        "/tmp/outside.json",
        "https://example.test/dag.json",
        "~/dag.json",
        "$DAG/dag.json",
        "fragments/*.json",
        "missing.json",
        ".",
    ],
)
def test_include_refuses_unsafe_missing_or_nonfile_paths(tmp_path: Path, included: str) -> None:
    _write(
        tmp_path / "root.json",
        {"include": [{"path": included, "namespace": "x"}], "steps": []},
    )
    with pytest.raises(DagJsonError, match="include path"):
        dag_from_path(tmp_path / "root.json")


def test_include_refuses_escape_after_symlink_resolution(tmp_path: Path) -> None:
    outside = tmp_path.parent / f"{tmp_path.name}-outside.json"
    _write(outside, {"steps": []})
    try:
        (tmp_path / "escape.json").symlink_to(outside)
        _write(
            tmp_path / "root.json",
            {"include": [{"path": "escape.json", "namespace": "x"}], "steps": []},
        )
        with pytest.raises(DagJsonError, match="resolves outside"):
            dag_from_path(tmp_path / "root.json")
    finally:
        outside.unlink(missing_ok=True)


def test_symlink_alias_extension_selects_source_syntax(tmp_path: Path) -> None:
    (tmp_path / "leaf.data").write_text(
        "steps: [{group: g, job: j, cmd: echo yaml}]\n", encoding="utf-8"
    )
    (tmp_path / "leaf.yaml").symlink_to(tmp_path / "leaf.data")
    (tmp_path / "root.data").write_text(
        "include: [{path: leaf.yaml, namespace: child}]\nsteps: []\n",
        encoding="utf-8",
    )
    (tmp_path / "root.yaml").symlink_to(tmp_path / "root.data")
    assert [step.tag for step in dag_from_path(tmp_path / "root.yaml").steps] == [
        "child.g.j"
    ]


def test_include_refuses_resource_and_global_policy_conflicts(tmp_path: Path) -> None:
    _write(
        tmp_path / "child.json",
        {"resource_caps": {"browser": 2}, "default_step_timeout": 9, "steps": []},
    )
    _write(
        tmp_path / "root.json",
        {
            "include": [{"path": "child.json", "namespace": "x"}],
            "resource_caps": {"browser": 1},
            "default_step_timeout": 10,
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError, match="global policy 'default_step_timeout' conflicts"):
        dag_from_path(tmp_path / "root.json")

    _write(tmp_path / "child.json", {"resource_caps": {"browser": 2}, "steps": []})
    with pytest.raises(DagJsonError, match="resource cap 'browser'=2 conflicts"):
        dag_from_path(tmp_path / "root.json")


def test_include_detects_cross_fragment_tag_collision_with_provenance(tmp_path: Path) -> None:
    _write(tmp_path / "a.json", {"steps": [{"group": "g", "job": "j", "cmd": "true"}]})
    _write(tmp_path / "b.json", {"steps": [{"group": "g", "job": "j", "cmd": "true"}]})
    _write(
        tmp_path / "root.json",
        {
            "include": [
                {"path": "a.json", "namespace": "same"},
                {"path": "b.json", "namespace": "same"},
            ],
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError) as raised:
        dag_from_path(tmp_path / "root.json")
    message = str(raised.value)
    assert "duplicate step tag 'same.g.j'" in message
    assert "a.json steps[0]" in message
    assert "b.json steps[0]" in message


def test_include_after_unknown_outer_tag_is_refused_by_flattened_graph(tmp_path: Path) -> None:
    _write(tmp_path / "child.json", {"steps": [{"group": "g", "job": "j", "cmd": "true"}]})
    _write(
        tmp_path / "root.json",
        {
            "include": [
                {"path": "child.json", "namespace": "child", "after": ["missing.step"]}
            ],
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError, match="after references missing outer step 'missing.step'"):
        dag_from_path(tmp_path / "root.json")

    _write(tmp_path / "child.json", {"steps": []})
    with pytest.raises(DagJsonError, match="after references missing outer step 'missing.step'"):
        dag_from_path(tmp_path / "root.json")


def test_include_after_must_reference_an_outer_step(tmp_path: Path) -> None:
    _write(tmp_path / "child.json", {"steps": [{"group": "g", "job": "j", "cmd": "true"}]})
    _write(
        tmp_path / "root.json",
        {
            "include": [
                {"path": "child.json", "namespace": "child", "after": ["child.g.j"]}
            ],
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError, match="after must name outer steps"):
        dag_from_path(tmp_path / "root.json")


def test_hostile_include_path_and_bytes_fail_as_dag_errors(tmp_path: Path) -> None:
    _write(
        tmp_path / "root.json",
        {"include": [{"path": "nul\x00.json", "namespace": "x"}], "steps": []},
    )
    with pytest.raises(DagJsonError, match="cannot resolve include path"):
        dag_from_path(tmp_path / "root.json")

    (tmp_path / "invalid.json").write_bytes(b"\xff")
    _write(
        tmp_path / "root.json",
        {"include": [{"path": "invalid.json", "namespace": "x"}], "steps": []},
    )
    with pytest.raises(DagJsonError, match="cannot read DAG"):
        dag_from_path(tmp_path / "root.json")

    (tmp_path / "loop.json").symlink_to("loop.json")
    _write(
        tmp_path / "root.json",
        {"include": [{"path": "loop.json", "namespace": "x"}], "steps": []},
    )
    with pytest.raises(DagJsonError, match="cannot resolve include path"):
        dag_from_path(tmp_path / "root.json")


def test_include_depth_limit_accepts_boundary_and_refuses_next(tmp_path: Path) -> None:
    for index in range(MAX_DAG_INCLUDE_DEPTH + 1):
        document: dict[str, object] = {"steps": []}
        if index < MAX_DAG_INCLUDE_DEPTH:
            document["include"] = [
                {"path": f"depth-{index + 1}.json", "namespace": f"n{index + 1}"}
            ]
        _write(tmp_path / f"depth-{index}.json", document)
    dag_from_path(tmp_path / "depth-0.json")

    _write(tmp_path / f"depth-{MAX_DAG_INCLUDE_DEPTH + 1}.json", {"steps": []})
    _write(
        tmp_path / f"depth-{MAX_DAG_INCLUDE_DEPTH}.json",
        {
            "include": [
                {
                    "path": f"depth-{MAX_DAG_INCLUDE_DEPTH + 1}.json",
                    "namespace": "overflow",
                }
            ],
            "steps": [],
        },
    )
    with pytest.raises(DagJsonError, match=f"maximum depth {MAX_DAG_INCLUDE_DEPTH}"):
        dag_from_path(tmp_path / "depth-0.json")


def test_include_instance_limit_accepts_boundary_and_refuses_next(tmp_path: Path) -> None:
    _write(tmp_path / "empty.json", {"steps": []})
    includes = [
        {"path": "empty.json", "namespace": f"n{index}"}
        for index in range(MAX_DAG_INCLUDE_INSTANCES)
    ]
    _write(tmp_path / "root.json", {"include": includes, "steps": []})
    dag_from_path(tmp_path / "root.json")

    includes.append({"path": "empty.json", "namespace": "overflow"})
    _write(tmp_path / "root.json", {"include": includes, "steps": []})
    with pytest.raises(DagJsonError, match=f"{MAX_DAG_INCLUDE_INSTANCES} include instances"):
        dag_from_path(tmp_path / "root.json")


def test_flattened_step_limit_accepts_boundary_and_refuses_next(tmp_path: Path) -> None:
    # Invalid placeholder steps keep the boundary case cheap: reaching the ordinary steps[0]
    # schema error proves the path loader did not reject the allowed count as oversized.
    _write(tmp_path / "root.json", {"steps": [None] * MAX_DAG_FLATTENED_STEPS})
    with pytest.raises(DagJsonError) as boundary:
        dag_from_path(tmp_path / "root.json")
    assert "steps[0]: expected an object" in str(boundary.value)
    assert "flattened steps" not in str(boundary.value)

    _write(tmp_path / "root.json", {"steps": [None] * (MAX_DAG_FLATTENED_STEPS + 1)})
    with pytest.raises(DagJsonError, match=f"{MAX_DAG_FLATTENED_STEPS} flattened steps"):
        dag_from_path(tmp_path / "root.json")
