"""Tests for dagrun.io (JSON round-trip + strict parse errors)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from dagrun.io import DagJsonError, dag_from_json, dag_to_json
from dagrun.model import (
    DagConfig,
    DagManifest,
    ResourceHint,
    Step,
    StepClass,
    StructuredTestResultsManifest,
    WriteDomainGuarantee,
    WriteDomainPolicy,
    result_manifest_owner,
    resolved_wall_timeout,
)
from dagrun.scheduler import run_dag


#: Multi-line description with quotes, backslashes, a tab, and unicode — proves the JSON string
#: escaping survives a round-trip (and, in the cross harness, is byte-identical to Rust).
_HAIRY_DESC = 'line 1\nline 2 with "quotes" and \\backslash\\ and\ttab and unicode é☃'


def _cfg() -> DagConfig:
    return DagConfig(
        steps=(
            Step(
                "build",
                "app",
                "compile",
                "make build",
                description=_HAIRY_DESC,
                labels=["quick", "full"],
                hint=ResourceHint(
                    est_duration_s=90.0,
                    rss_baseline_bytes=5 * 1024**3,
                    classification=StepClass.CPU_BOUND,
                    preferred_inner_jobs=8,
                ),
            ),
            Step(
                "e2e",
                "smoke",
                "browser",
                "make e2e",
                deps=["build.app"],
                env={"HEADLESS": "1"},
                hint=ResourceHint(resources={"browser": 1}, classification=StepClass.LATENCY_BOUND),
            ),
        ),
        description="the whole pipeline",
        resource_caps={"browser": 2},
        mem_cap_factor=1.25,
        outer_mem_safety_factor=1.1,
    )


def test_roundtrip_is_stable() -> None:
    cfg = _cfg()
    once = dag_to_json(cfg)
    twice = dag_to_json(dag_from_json(once))
    assert once == twice  # canonical JSON is a fixed point
    back = dag_from_json(once)
    assert [s.tag for s in back.steps] == ["build.app", "e2e.smoke"]
    assert back.description == "the whole pipeline"
    assert back.steps[0].description == _HAIRY_DESC
    assert back.steps[0].labels == ["quick", "full"]
    assert back.steps[1].labels == []
    assert back.steps[1].description == ""  # default empty
    assert back.resource_caps == {"browser": 2}
    assert back.steps[0].hint.classification is StepClass.CPU_BOUND
    assert back.steps[0].hint.rss_baseline_bytes == 5 * 1024**3
    assert back.steps[1].hint.resources == {"browser": 1}
    assert back.steps[1].env == {"HEADLESS": "1"}


def test_minimal_document_defaults() -> None:
    cfg = dag_from_json('{"steps": [{"group": "g", "job": "j", "cmd": "true"}]}')
    step = cfg.steps[0]
    assert step.tag == "g.j"
    assert step.desc == "" and step.deps == [] and step.env == {}
    assert step.description == "" and cfg.description == ""  # default empty
    assert step.labels == []
    assert step.timeout == 0  # the "derive it" sentinel, not "no bound"
    assert resolved_wall_timeout(step, cfg.default_step_timeout) == 1800
    assert step.cpu_timeout == 0  # CPU-time guard disabled by default
    assert step.hint.classification is StepClass.LIGHT
    assert step.manifest is None
    assert step.integration_test_binaries is None
    assert cfg.resource_caps == {} and cfg.mem_cap_factor == 1.25


@pytest.mark.parametrize(
    ("labels", "message"),
    [
        ('"quick"', "field 'labels' must be a list of strings"),
        ('["quick", 1]', "field 'labels' must contain only strings"),
        ('[""]', "labels must be non-empty"),
        ('["quick", "quick"]', "duplicate labels are not allowed"),
    ],
)
def test_labels_refuse_malformed_values(labels: str, message: str) -> None:
    with pytest.raises(DagJsonError, match=message):
        dag_from_json(
            '{"steps":[{"group":"g","job":"j","cmd":"true","labels":'
            + labels
            + "}]}"
        )


def test_manifest_selection_roundtrips_and_refuses_malformed_values() -> None:
    doc = (
        '{"steps":[{"group":"e2e","job":"manifest_applications","cmd":"true",'
        '"manifest":{"lane":"portable","category":"applications"}}]}'
    )
    cfg = dag_from_json(doc)
    assert cfg.steps[0].manifest == DagManifest(lane="portable", category="applications")
    assert cfg.steps[0].result_manifests is None
    encoded = dag_to_json(cfg)
    assert dag_to_json(dag_from_json(encoded)) == encoded

    malformed = [
        ('{"lane":"portable"}', "manifest: field 'category' must be a string"),
        ('{"lane":"","category":"applications"}', "manifest.lane: must be non-empty"),
        (
            '{"lane":"portable","category":"applications","future":"value"}',
            "manifest: unknown field(s) 'future'",
        ),
    ]
    for value, message in malformed:
        with pytest.raises(DagJsonError) as raised:
            dag_from_json(
                '{"steps":[{"group":"e2e","job":"manifest_applications",'
                f'"cmd":"true","manifest":{value}}}]}}'
            )
        assert message in str(raised.value)


def test_result_manifests_roundtrip_preserves_absent_null_and_explicit_empty() -> None:
    doc = """{"steps":[
        {"group":"e2e","job":"legacy","cmd":"true",
         "manifest":{"lane":"portable","category":"applications"}},
        {"group":"e2e","job":"null","cmd":"true",
         "manifest":{"lane":"portable","category":"applications"},
         "result_manifests":null},
        {"group":"e2e","job":"none","cmd":"true",
         "manifest":{"lane":"portable","category":"applications"},
         "result_manifests":[]},
        {"group":"e2e","job":"many","cmd":"true","result_manifests":[
            {"lane":"portable","category":"applications","mode":"verify","backend":"ptrace"},
            {"lane":"portable","category":"c-programs","test":"c-programs/add-key-enosys",
             "mode":"run","backend":"kvm"}
         ]}
    ]}"""
    cfg = dag_from_json(doc)
    assert cfg.steps[0].result_manifests is None
    assert cfg.steps[0].effective_result_manifests() == (
        DagManifest(lane="portable", category="applications"),
    )
    assert cfg.steps[1].result_manifests is None
    assert cfg.steps[1].effective_result_manifests() == (
        DagManifest(lane="portable", category="applications"),
    )
    assert cfg.steps[2].result_manifests == []
    assert cfg.steps[2].effective_result_manifests() == ()
    assert cfg.steps[3].effective_result_manifests()[-1] == DagManifest(
        lane="portable",
        category="c-programs",
        test="c-programs/add-key-enosys",
        mode="run",
        backend="kvm",
    )
    encoded = dag_to_json(cfg)
    encoded_steps = json.loads(encoded)["steps"]
    assert "result_manifests" not in encoded_steps[0]
    assert "result_manifests" not in encoded_steps[1]
    assert encoded_steps[2]["result_manifests"] == []
    assert dag_to_json(dag_from_json(encoded)) == encoded


@pytest.mark.parametrize(
    ("value", "message"),
    [
        (
            '{"lane":"portable","category":"applications"}',
            "result_manifests: must be a list of result declarations or null",
        ),
        ("[null]", "result_manifests[0]: expected an object"),
        (
            '[{"lane":"portable"}]',
            "result_manifests[0]: field 'category' must be a string",
        ),
        (
            '[{"lane":"portable","category":"applications","mode":""}]',
            "result_manifests[0].mode: must be non-empty when present",
        ),
        (
            '[{"lane":"portable","category":"applications"},'
            '{"lane":"portable","category":"applications"}]',
            "result_manifests: duplicate selector at index 1",
        ),
        (
            '[{"lane":"portable","category":"applications","future":1}]',
            "result_manifests[0]: unknown field(s) 'future'",
        ),
    ],
)
def test_result_manifests_refuse_malformed_and_duplicate_selectors(
    value: str, message: str
) -> None:
    with pytest.raises(DagJsonError) as raised:
        dag_from_json(
            '{"steps":[{"group":"e2e","job":"results","cmd":"true",'
            f'"result_manifests":{value}}}]}}'
        )
    assert message in str(raised.value)


def test_structured_result_manifest_roundtrips_with_legacy_cell_selector() -> None:
    doc = """{"steps":[{"group":"test","job":"counts","cmd":"true",
        "result_manifests":[
          {"lane":"portable","category":"applications"},
          {"kind":"structured-test-results","schema":2,
           "path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"}
        ]}]}"""
    cfg = dag_from_json(doc)
    assert cfg.steps[0].effective_result_manifests() == (
        DagManifest(lane="portable", category="applications"),
    )
    assert cfg.steps[0].structured_test_results_manifest() == (
        StructuredTestResultsManifest.current("test.counts")
    )
    encoded = dag_to_json(cfg)
    encoded_declarations = json.loads(encoded)["steps"][0]["result_manifests"]
    assert encoded_declarations[0] == {
        "lane": "portable",
        "category": "applications",
    }
    assert encoded_declarations[1] == {
        "kind": "structured-test-results",
        "schema": 2,
        "path_env": "DAGRUN_TEST_COUNTS_PATH",
        "owner": "test.counts",
    }
    assert dag_to_json(dag_from_json(encoded)) == encoded


@pytest.mark.parametrize(
    ("declaration", "message"),
    [
        ({"kind": "future", "schema": 3, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "test.counts"}, "unknown result-manifest kind"),
        ({"kind": "structured-test-results", "schema": "3", "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "test.counts"}, "schema: must be an integer"),
        ({"kind": "structured-test-results", "schema": 5, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "test.counts"}, "got 5"),
        ({"kind": "structured-test-results", "schema": 2, "path_env": "OTHER", "owner": "test.counts"}, "path_env: structured test results require"),
        ({"kind": "structured-test-results", "schema": 2, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": ""}, "owner: must be non-empty"),
        ({"kind": "structured-test-results", "schema": 2, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "test.counts", "future": 1}, "unknown field(s) 'future'"),
        ({"kind": "structured-test-results", "schema": 2, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "other.step"}, "owner 'other.step' must equal the containing step tag"),
    ],
)
def test_structured_result_manifest_refuses_wrong_contract(
    declaration: dict[str, object], message: str
) -> None:
    with pytest.raises(DagJsonError) as raised:
        dag_from_json(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "test",
                            "job": "counts",
                            "cmd": "true",
                            "result_manifests": [declaration],
                        }
                    ]
                }
            )
        )
    assert message in str(raised.value)


def test_result_manifest_owner_refuses_missing_inexact_and_cross_node_ownership() -> None:
    result = DagManifest(
        lane="portable",
        category="applications",
        test="applications/date",
        mode="verify",
        backend="ptrace",
    )
    legacy = Step(
        "e2e",
        "legacy",
        "legacy owner",
        "true",
        manifest=DagManifest(lane="portable", category="applications"),
    )
    assert result_manifest_owner([legacy], result) is legacy

    explicit_empty = Step(
        "e2e",
        "none",
        "no results",
        "true",
        manifest=DagManifest(lane="portable", category="applications"),
        result_manifests=[],
    )
    with pytest.raises(ValueError, match="has no owning step"):
        result_manifest_owner([explicit_empty], result)

    with pytest.raises(ValueError, match="missing test, mode, backend"):
        result_manifest_owner(
            [legacy], DagManifest(lane="portable", category="applications")
        )

    for result_with_empty_dimension, missing in (
        (
            DagManifest(
                lane="",
                category="applications",
                test="applications/date",
                mode="verify",
                backend="ptrace",
            ),
            "lane",
        ),
        (
            DagManifest(
                lane="portable",
                category="",
                test="applications/date",
                mode="verify",
                backend="ptrace",
            ),
            "category",
        ),
    ):
        with pytest.raises(ValueError, match=f"missing {missing}"):
            result_manifest_owner([legacy], result_with_empty_dimension)

    exact_owner = Step(
        "e2e",
        "exact",
        "exact owner",
        "true",
        result_manifests=[result],
    )
    with pytest.raises(ValueError, match="multiple owning steps.*e2e.legacy, e2e.exact"):
        result_manifest_owner([legacy, exact_owner], result)


def test_integration_test_binaries_roundtrip_and_refuse_malformed_values() -> None:
    doc = (
        '{"steps":[{"group":"test","job":"cli","cmd":"true",'
        '"integration_test_binaries":["unit_alpha","unit_beta"]}]}'
    )
    cfg = dag_from_json(doc)
    assert cfg.steps[0].integration_test_binaries == ["unit_alpha", "unit_beta"]
    encoded = dag_to_json(cfg)
    assert dag_to_json(dag_from_json(encoded)) == encoded

    for value, message in [
        ('"unit_alpha"', "field 'integration_test_binaries' must be a list of strings"),
        (
            '["unit_alpha",7]',
            "field 'integration_test_binaries' must contain only strings",
        ),
        (
            '["unit_alpha",""]',
            "field 'integration_test_binaries' must not contain empty names",
        ),
        (
            '["unit_alpha","unit_alpha"]',
            "field 'integration_test_binaries' contains duplicate name 'unit_alpha'",
        ),
    ]:
        with pytest.raises(DagJsonError) as raised:
            dag_from_json(
                '{"steps":[{"group":"test","job":"cli","cmd":"true",'
                f'"integration_test_binaries":{value}}}]}}'
            )
        assert message in str(raised.value)


def test_cpu_timeout_roundtrip_and_conditional_emit() -> None:
    # A step with a CPU-time budget parses it and round-trips; a step without one omits the
    # key entirely, so existing DAGs stay byte-for-byte unchanged (absence parses back to 0).
    doc = (
        '{"steps": ['
        '{"group": "g", "job": "cpu", "cmd": "true", "cpu_timeout": 30},'
        '{"group": "g", "job": "nocpu", "cmd": "true"}]}'
    )
    cfg = dag_from_json(doc)
    by_tag = cfg.by_tag()
    assert by_tag["g.cpu"].cpu_timeout == 30
    assert by_tag["g.nocpu"].cpu_timeout == 0  # absent -> default 0

    out = dag_to_json(cfg)
    assert '"cpu_timeout": 30' in out  # present only for the step that set it
    # Exactly one step emits the key; the default-0 step must not.
    assert out.count('"cpu_timeout"') == 1
    # Stable across a second serialization pass.
    assert dag_to_json(dag_from_json(out)) == out


def test_delegated_children_is_explicit_and_roundtrips() -> None:
    cfg = dag_from_json(
        '{"steps":['
        '{"group":"g","job":"delegating","cmd":"true","delegated_children":true},'
        '{"group":"g","job":"ordinary","cmd":"true"}]}'
    )
    assert cfg.steps[0].delegated_children
    assert not cfg.steps[1].delegated_children
    encoded = dag_to_json(cfg)
    assert encoded.count('"delegated_children"') == 1
    assert dag_to_json(dag_from_json(encoded)) == encoded

    with pytest.raises(DagJsonError, match="delegated_children.*boolean"):
        dag_from_json(
            '{"steps":[{"group":"g","job":"bad","cmd":"true",'
            '"delegated_children":"yes"}]}'
        )


def test_fail_fast_family_roundtrip_and_rejects_empty() -> None:
    doc = (
        "{\"steps\": ["
        "{\"group\":\"g\",\"job\":\"scoped\",\"cmd\":\"true\","
        "\"fail_fast_family\":\"family-a\"},"
        "{\"group\":\"g\",\"job\":\"global\",\"cmd\":\"true\"}]}"
    )
    cfg = dag_from_json(doc)
    assert cfg.steps[0].fail_fast_family == "family-a"
    assert cfg.steps[1].fail_fast_family is None

    encoded = dag_to_json(cfg)
    assert encoded.count("\"fail_fast_family\"") == 1
    assert dag_to_json(dag_from_json(encoded)) == encoded

    with pytest.raises(DagJsonError, match="fail_fast_family: must be non-empty"):
        dag_from_json(
            "{\"steps\":[{\"group\":\"g\",\"job\":\"j\",\"cmd\":\"true\","
            "\"fail_fast_family\":\"   \"}]}"
        )


def test_default_step_timeout_applied() -> None:
    # A step that omits `timeout` inherits the document-level default_step_timeout; a step
    # with its own timeout keeps it; and the config records the document default.
    doc = (
        '{"default_step_timeout": 42, "steps": ['
        '{"group": "g", "job": "a", "cmd": "true"},'
        '{"group": "g", "job": "b", "cmd": "true", "timeout": 7}]}'
    )
    cfg = dag_from_json(doc)
    by_tag = cfg.by_tag()
    assert by_tag["g.a"].timeout == 42  # inherited document default
    assert by_tag["g.b"].timeout == 7  # explicit per-step timeout wins
    assert cfg.default_step_timeout == 42


def test_default_step_timeout_falls_back_to_module_constant() -> None:
    # Without a document-level default, NOTHING is materialized: both stay at the 0 "derive it"
    # sentinel, and the module constant reappears only as the RESOLVED bound. Materializing 1800
    # here is what baked a load-sensitive number into every graph.
    cfg = dag_from_json('{"steps": [{"group": "g", "job": "a", "cmd": "true"}]}')
    assert cfg.steps[0].timeout == 0
    assert cfg.default_step_timeout == 0
    assert resolved_wall_timeout(cfg.steps[0], cfg.default_step_timeout) == 1800


def test_typed_intentional_skip_roundtrips_and_rejects_dependents() -> None:
    leaf = dag_from_json(
        '{"steps":[{"group":"g","job":"empty","cmd":"false",'
        '"skip_reason":"empty-manifest-bucket"}]}'
    )
    assert leaf.steps[0].skip_reason is not None
    assert '"skip_reason": "empty-manifest-bucket"' in dag_to_json(leaf)

    bad_docs = [
        '{"steps":[{"group":"g","job":"x","cmd":"true",'
        '"skip_reason":"unknown"}]}',
        '{"steps":['
        '{"group":"g","job":"empty","cmd":"false",'
        '"skip_reason":"empty-manifest-bucket"},'
        '{"group":"g","job":"consumer","cmd":"true","deps":["g.empty"]}]}'
    ]
    for doc in bad_docs:
        try:
            dag_from_json(doc)
        except DagJsonError:
            pass
        else:
            raise AssertionError(f"intentional-skip misuse accepted: {doc}")


def test_strict_parse_errors() -> None:
    bad_docs = [
        "not json at all",
        "[]",  # root not an object
        '{"steps": "not a list"}',
        '{"steps": [{"job": "j", "cmd": "c"}]}',  # missing group
        '{"steps": [{"group": "g", "job": "j"}]}',  # missing cmd
        '{"steps": [{"group": "g", "job": "j", "cmd": "c", "timeout": "x"}]}',
        '{"steps": [{"group": "g", "job": "j", "cmd": "c", "deps": [1]}]}',
        '{"steps": [{"group": "g", "job": "j", "cmd": "c", "hint": {"classification": "nope"}}]}',
        '{"steps": [{"group": "g", "job": "j", "cmd": "c", "hint": {"resources": {"x": "y"}}}]}',
    ]
    for doc in bad_docs:
        raised = False
        try:
            dag_from_json(doc)
        except DagJsonError:
            raised = True
        assert raised, f"expected DagJsonError for: {doc!r}"


def test_a_non_dag_document_names_what_was_read_and_what_to_do() -> None:
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json('{"schema": 2, "bucket": "example", "test": []}')
    assert str(excinfo.value) == (
        "<root>: expected a dagrun DAG document with a top-level 'steps' list; found no "
        "'steps' key (top-level keys: 'bucket', 'schema', 'test'). This may be a different "
        "document type. Pass a dagrun DAG file, or run `dagrun quickstart` for the schema."
    )

    with pytest.raises(DagJsonError) as wrong_type:
        dag_from_json('{"steps": "not a list"}')
    assert str(wrong_type.value) == (
        "<root>: expected a dagrun DAG document with a top-level 'steps' list; found "
        "'steps' with type str (top-level keys: 'steps'). This may be a different document "
        "type. Pass a dagrun DAG file, or run `dagrun quickstart` for the schema."
    )


def test_write_domain_policy_roundtrip_and_fail_closed_parse() -> None:
    good = (
        '{"steps": ['
        '{"group":"g","job":"reader","cmd":"true","write_domains":[]},'
        '{"group":"g","job":"barrier","cmd":"true",'
        '"write_domains":["shared-cargo-target"],'
        '"write_domain_guarantee":"immutable-artifact-barrier"},'
        '{"group":"g","job":"shielded","cmd":"true","deps":["g.barrier"],'
        '"write_domains":["shared-cargo-target"],'
        '"write_domain_guarantee":"artifact-barrier-dependent"},'
        '{"group":"g","job":"writer","cmd":"true",'
        '"write_domains":["isolated-target"],'
        '"write_domain_guarantee":"explicitly-isolated"}],'
        '"write_domain_policy":{"require_explicit":true,'
        '"allowed_domains":["shared-cargo-target","isolated-target"]}}'
    )
    cfg = dag_from_json(good)
    assert cfg.steps[0].write_domains == []
    assert cfg.steps[3].write_domains == ["isolated-target"]
    assert (
        cfg.steps[3].write_domain_guarantee
        is WriteDomainGuarantee.EXPLICITLY_ISOLATED
    )
    encoded = dag_to_json(cfg)
    assert dag_to_json(dag_from_json(encoded)) == encoded

    bad_documents = (
        # Missing is distinct from explicitly read-only [] and must refuse.
        '{"steps":[{"group":"g","job":"j","cmd":"true"}],'
        '"write_domain_policy":{"require_explicit":true,"allowed_domains":[]}}',
        # Closed vocabulary: a typo cannot silently create a new domain.
        '{"steps":[{"group":"g","job":"j","cmd":"true",'
        '"write_domains":["typo"],"write_domain_guarantee":"artifact-producer"}],'
        '"write_domain_policy":{"require_explicit":true,'
        '"allowed_domains":["shared-cargo-target"]}}',
        # Naming barrier dependence is not enough: a transitive immutable barrier is mandatory.
        '{"steps":[{"group":"g","job":"j","cmd":"true",'
        '"write_domains":["shared-cargo-target"],'
        '"write_domain_guarantee":"artifact-barrier-dependent"}],'
        '"write_domain_policy":{"require_explicit":true,'
        '"allowed_domains":["shared-cargo-target"]}}',
        # Duplicate declarations and nonempty declarations without a guarantee refuse.
        '{"steps":[{"group":"g","job":"j","cmd":"true",'
        '"write_domains":["shared-cargo-target","shared-cargo-target"]}],'
        '"write_domain_policy":{"require_explicit":true,'
        '"allowed_domains":["shared-cargo-target"]}}',
    )
    for doc in bad_documents:
        with pytest.raises(DagJsonError, match="write-domain policy refused"):
            dag_from_json(doc)


def test_scheduler_rechecks_write_domains_before_spawning(tmp_path: Path) -> None:
    marker = tmp_path / "ran"
    cfg = DagConfig(
        steps=(Step("g", "j", "", f"touch {marker}"),),
        write_domain_policy=WriteDomainPolicy(
            require_explicit=True, allowed_domains=frozenset({"target-ci"})
        ),
    )
    result = run_dag(cfg, jobs=1)
    assert not result.ok
    assert result.outcomes == ()
    assert not marker.exists(), "policy refusal happened after the node wrote"


# ------------------------------------------------- the loader's graph contract (closed schema,
# duplicate tags, missing deps, cycles, unsatisfiable demands)
#
# Every case below was ACCEPTED by this loader before, and the first four then produced a wrong
# run rather than an error: an ignored field, a step that silently never ran, a starve discovered
# only after unrelated work had completed, and a RecursionError (a stack-overflow core dump in the
# Rust edition).


def test_an_unknown_step_field_is_refused_and_named_with_its_step() -> None:
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json('{"steps":[{"group":"a","job":"one","cmd":"true","bogus_field":42}]}')
    error = str(excinfo.value)
    assert "steps[0] (a.one)" in error, error
    assert "'bogus_field'" in error, error
    # The known-field list is part of the message: the whole point is that the author meant one
    # of them.
    assert "cpu_timeout" in error, error

    # Two at once are named together, sorted, so a reader fixes both in one pass.
    with pytest.raises(DagJsonError, match=r"'alpha', 'zeta'"):
        dag_from_json('{"steps":[{"group":"a","job":"one","cmd":"true","zeta":1,"alpha":2}]}')

    # The nested schema objects are closed too. `est_duration` for `est_duration_s` is the
    # canonical silent drop: the estimate is simply not carried and planning uses 0.
    with pytest.raises(DagJsonError) as hint_info:
        dag_from_json(
            '{"steps":[{"group":"a","job":"one","cmd":"true","hint":{"est_duration":9}}]}'
        )
    assert "steps[0].hint" in str(hint_info.value)
    assert "'est_duration'" in str(hint_info.value)

    with pytest.raises(DagJsonError) as policy_info:
        dag_from_json('{"steps":[],"write_domain_policy":{"require_explicits":true}}')
    assert "write_domain_policy" in str(policy_info.value)
    assert "'require_explicits'" in str(policy_info.value)


def test_a_step_declaring_only_known_fields_still_loads() -> None:
    # The other side, so "refuse every step" cannot pass the case above.
    dag_from_json(
        '{"steps":[{"group":"a","job":"one","desc":"d","description":"long",'
        '"cmd":"true","deps":[],"env":{"K":"V"},"networkonly":false,'
        '"engine_only":false,"delegated_children":false,"timeout":5,"cpu_timeout":3,'
        '"cmdtype":"generic-with-flag",'
        '"jobs_flag":"-j",'
        '"jobs_env":"J","explains":[],"fail_fast_family":"fam",'
        '"hint":{"resources":{},"est_duration_s":1.0,"classification":"light"}}]}'
    )


def test_a_duplicate_step_tag_is_refused_rather_than_silently_dropping_a_step() -> None:
    # THE SILENT ONE. Two steps, one tag: the runner executed exactly one of them and then
    # reported "2 passed". Nothing anywhere said a declared command had not been run.
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json(
            '{"steps":[{"group":"a","job":"one","cmd":"echo FIRST"},'
            '{"group":"a","job":"one","cmd":"echo SECOND"}]}'
        )
    error = str(excinfo.value)
    assert "duplicate step tag 'a.one'" in error, error
    assert "declared 2 times" in error, error
    assert "vanish silently" in error, error


def test_a_missing_dependency_is_refused_at_load_not_after_a_full_build() -> None:
    with pytest.raises(
        DagJsonError, match=r"step a\.one: depends on 'b\.missing', which no step declares"
    ):
        dag_from_json(
            '{"steps":[{"group":"z","job":"zero","cmd":"true"},'
            '{"group":"a","job":"one","cmd":"true","deps":["b.missing"]}]}'
        )


def test_a_dependency_cycle_is_refused_and_the_refusal_names_the_cycle() -> None:
    # The CRASH case. Accepted by the loader, this reached the bottom-level walk and raised
    # RecursionError; the Rust edition aborted the process with a stack overflow (core dump).
    with pytest.raises(DagJsonError, match=r"dependency cycle: a\.one -> b\.two -> a\.one"):
        dag_from_json(
            '{"steps":[{"group":"a","job":"one","cmd":"true","deps":["b.two"]},'
            '{"group":"b","job":"two","cmd":"true","deps":["a.one"]}]}'
        )

    # A self-edge is a one-node cycle and is named the same way.
    with pytest.raises(DagJsonError, match=r"dependency cycle: a\.one -> a\.one"):
        dag_from_json('{"steps":[{"group":"a","job":"one","cmd":"true","deps":["a.one"]}]}')

    # A long chain is walked ITERATIVELY, so the cycle check cannot itself be the crash: this
    # graph is far deeper than CPython's default recursion limit.
    entries = ",".join(
        '{"group":"g","job":"s%d","cmd":"true","deps":["g.s%d"]}'
        % (i, 4999 if i == 0 else i - 1)
        for i in range(5000)
    )
    with pytest.raises(DagJsonError, match=r"dependency cycle: "):
        dag_from_json('{"steps":[%s]}' % entries)


def test_a_demand_above_a_positive_cap_is_refused_but_a_cap_of_zero_stays_a_deliberate_block() -> None:
    with pytest.raises(
        DagJsonError,
        match=(
            r"step a\.one: demands browser=2 but resource_caps declares browser=1, "
            r"so it can never be admitted"
        ),
    ):
        dag_from_json(
            '{"resource_caps":{"browser":1},'
            '"steps":[{"group":"a","job":"one","cmd":"true",'
            '"hint":{"resources":{"browser":2}}}]}'
        )

    # A cap of exactly 0 is documented as "blocked on purpose", so it is NOT a load error.
    # Asserting the boundary from both sides is what stops this becoming a blanket ban.
    dag_from_json(
        '{"resource_caps":{"browser":0},'
        '"steps":[{"group":"a","job":"one","cmd":"true",'
        '"hint":{"resources":{"browser":1}}}]}'
    )

    # An intentionally-skipped step never launches, so its dormant demand is not an error.
    dag_from_json(
        '{"resource_caps":{"browser":1},'
        '"steps":[{"group":"a","job":"one","cmd":"true",'
        '"skip_reason":"empty-manifest-bucket",'
        '"hint":{"resources":{"browser":9}}}]}'
    )


def test_a_graph_with_several_faults_reports_them_all_at_once() -> None:
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json(
            '{"resource_caps":{"browser":1},'
            '"steps":[{"group":"a","job":"one","cmd":"true","deps":["nope.gone"]},'
            '{"group":"b","job":"two","cmd":"true","deps":["c.three"]},'
            '{"group":"c","job":"three","cmd":"true","deps":["b.two"]},'
            '{"group":"d","job":"four","cmd":"true",'
            '"hint":{"resources":{"browser":5}}}]}'
        )
    error = str(excinfo.value)
    assert "3 graph error(s)" in error, error
    assert "'nope.gone'" in error, error
    assert "dependency cycle: b.two -> c.three -> b.two" in error, error
    assert "demands browser=5" in error, error


def test_a_duplicate_tag_short_circuits_the_edge_checks() -> None:
    # While two steps share a tag, every statement about "the step named X" is ambiguous, so the
    # loader reports the ambiguity and nothing built on top of it.
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json(
            '{"steps":[{"group":"a","job":"one","cmd":"true","deps":["nope.gone"]},'
            '{"group":"a","job":"one","cmd":"true"}]}'
        )
    error = str(excinfo.value)
    assert "1 graph error(s)" in error, error
    assert "duplicate step tag" in error, error
    assert "nope.gone" not in error, error


def test_classified_result_manifest_is_explicit_and_roundtrips() -> None:
    doc = {"steps": [{"group": "test", "job": "counts", "cmd": "true", "result_manifests": [
        {"kind": "structured-test-results", "schema": 3, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "test.counts"}
    ]}]}
    cfg = dag_from_json(json.dumps(doc))
    assert cfg.steps[0].structured_test_results_manifest() == StructuredTestResultsManifest.classified("test.counts")
    assert StructuredTestResultsManifest.current("test.counts").schema == 2
    assert StructuredTestResultsManifest.retained_schema2("test.counts").schema == 2
    encoded = dag_to_json(cfg)
    assert json.loads(encoded)["steps"][0]["result_manifests"][0]["schema"] == 3
    assert dag_to_json(dag_from_json(encoded)) == encoded


def test_diagnostic_result_manifest_is_explicit_and_roundtrips() -> None:
    doc = {"steps": [{"group": "test", "job": "counts", "cmd": "true", "result_manifests": [
        {"kind": "structured-test-results", "schema": 4, "path_env": "DAGRUN_TEST_COUNTS_PATH", "owner": "test.counts"}
    ]}]}
    cfg = dag_from_json(json.dumps(doc))
    assert cfg.steps[0].structured_test_results_manifest() == StructuredTestResultsManifest.diagnostic("test.counts")
    assert StructuredTestResultsManifest.diagnostic("test.counts") != StructuredTestResultsManifest.classified("test.counts")
    encoded = dag_to_json(cfg)
    assert json.loads(encoded)["steps"][0]["result_manifests"][0]["schema"] == 4
    assert dag_to_json(dag_from_json(encoded)) == encoded
