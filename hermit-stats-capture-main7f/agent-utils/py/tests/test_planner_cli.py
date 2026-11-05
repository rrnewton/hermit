"""Tests for the pr-landing-planner CLI surface (in-process, stdlib capture)."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
from pathlib import Path

from pr_landing_planner import __version__
from pr_landing_planner.cli import PROG, _load_userguide, main


def _capture(args: list[str]) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        rc = main(args)
    return rc, out.getvalue(), err.getvalue()


def _example(name: str) -> str:
    return str(Path(__file__).resolve().parent.parent / "pr_landing_planner" / "examples" / name)


DEMO = _example("pr-landing-demo.yaml")
FLAKY = _example("flaky-signatures.yaml")


def test_no_args_prints_help() -> None:
    rc, out, _ = _capture([])
    assert rc == 0
    assert PROG in out and "quickstart" in out


def test_quickstart_is_self_contained() -> None:
    rc, out, _ = _capture(["quickstart"])
    assert rc == 0
    for marker in ("five red classifications", "Per-PR actions", "Output formats", "Exit codes", "Demo fixture"):
        assert marker in out
    assert "none,labels,command" in out
    assert "beads" not in out.lower()


def test_quickstart_emit_demo_is_loadable() -> None:
    rc, out, _ = _capture(["quickstart", "--emit-demo"])
    assert rc == 0 and "prs:" in out


def test_priority_hook_errors_are_controlled() -> None:
    rc, out, err = _capture(["plan", "--fixture", DEMO, "--priority-source", "command"])
    assert rc == 2 and out == ""
    assert "requires a non-empty command" in err

    rc, out, err = _capture(
        [
            "plan",
            "--fixture",
            DEMO,
            "--priority-source",
            "command",
            "--priority-cmd",
            "printf 9223372036854775808",
        ]
    )
    assert rc == 2 and out == ""
    assert "signed 64-bit ASCII integer" in err


def test_userguide_prints_embedded_guide() -> None:
    rc, out, _ = _capture(["--userguide"])
    assert rc == 0
    assert out == _load_userguide()
    assert len(out) > 3000
    assert "pr-landing-planner" in out


def test_plan_human_all_classes() -> None:
    rc, out, _ = _capture(
        [
            "plan",
            "--fixture",
            DEMO,
            "--flaky-signatures",
            FLAKY,
            "--freshness-max-behind",
            "0",
        ]
    )
    assert rc == 0
    assert "land-now" in out
    assert "rebase-then-land" in out
    assert "refire-stale-gate" in out
    assert "refire-ci" in out
    assert "escalate-runner-outage" in out
    assert "SYSTEMIC RUNNER OUTAGE" in out
    assert "Parallel-safe groups" in out


def test_plan_json_is_valid_and_has_schema() -> None:
    rc, out, _ = _capture(["plan", "--fixture", DEMO, "--flaky-signatures", FLAKY, "--format", "json"])
    assert rc == 0
    obj = json.loads(out)
    assert obj["repository"] == "OWNER/NAME"
    assert set(obj["plan"]) >= {"parallel_safe_groups", "land_now", "order", "per_pr_actions"}
    assert 1043 in obj["plan"]["land_now"]
    # Behind the base is not a reroute reason by default...
    behind = next(item for item in obj["plan"]["per_pr_actions"] if item["pr"] == 987)
    assert behind["action"] == "land-now"
    # ...but a caller that asks for a freshness bound still gets the reroute and
    # its wording, so this keeps that path covered rather than dropping it.
    rc_strict, out_strict, _ = _capture(
        ["plan", "--fixture", DEMO, "--flaky-signatures", FLAKY, "--format", "json",
         "--freshness-max-behind", "0"]
    )
    assert rc_strict == 0
    behind_strict = next(
        item for item in json.loads(out_strict)["plan"]["per_pr_actions"] if item["pr"] == 987
    )
    assert behind_strict["action"] == "rebase-then-land"
    assert "rebase before landing" in behind_strict["why"]
    assert 1049 in obj["diagnostics"]["flaky_reds"]
    assert obj["diagnostics"]["outage_suspected"] is True
    # Deterministic: identical bytes on a second run.
    _, out2, _ = _capture(["plan", "--fixture", DEMO, "--flaky-signatures", FLAKY, "--format", "json"])
    assert out == out2


def test_plan_archives_json_to_disk_when_archive_dir_set(tmp_path: Path) -> None:
    archive = tmp_path / "plans"
    rc, out, err = _capture(
        ["plan", "--fixture", DEMO, "--format", "json", "--archive-dir", str(archive)]
    )
    assert rc == 0
    # The path is announced on STDERR (stdout stays pure JSON) and the file really exists.
    assert "NOTE: plan archived to" in err
    written = sorted(archive.glob("plan-*.json"))
    assert len(written) == 1
    announced = err.split("NOTE: plan archived to", 1)[1].strip().splitlines()[0]
    assert Path(announced) == written[0]
    # The archived artifact is the canonical machine schema, byte-identical to stdout json.
    assert written[0].read_text(encoding="utf-8") == out


def test_plan_archives_even_for_human_format(tmp_path: Path) -> None:
    archive = tmp_path / "plans"
    rc, _, err = _capture(
        ["plan", "--fixture", DEMO, "--format", "human", "--archive-dir", str(archive)]
    )
    assert rc == 0
    written = sorted(archive.glob("plan-*.json"))
    assert len(written) == 1
    obj = json.loads(written[0].read_text(encoding="utf-8"))
    assert obj["repository"] == "OWNER/NAME"
    assert "NOTE: plan archived to" in err


def test_plan_no_archive_and_fixture_default_leave_no_files(tmp_path: Path) -> None:
    # --no-archive suppresses archiving even with an explicit dir.
    rc, _, err = _capture(
        ["plan", "--fixture", DEMO, "--archive-dir", str(tmp_path), "--no-archive"]
    )
    assert rc == 0
    assert list(tmp_path.glob("plan-*.json")) == []
    assert "NOTE: plan archived to" not in err
    # A bare --fixture run (no --archive-dir) archives nothing and stays stderr-clean.
    rc2, _, err2 = _capture(["plan", "--fixture", DEMO, "--format", "json"])
    assert rc2 == 0
    assert "NOTE: plan archived to" not in err2


def test_plan_context_exact_head_bypasses_stale_gate(tmp_path: Path) -> None:
    context = tmp_path / "landing-context.json"
    context.write_text(
        json.dumps(
            {
                "prs": [
                    {
                        "pr": 942,
                        "head_sha": "sha-942",
                        "base_sha": "basesha-integration",
                        "validation_evidence": "clean-validate-record",
                        "validation_authority": "hard-green",
                        "policy_class": "ci-hygiene",
                        "assigned_agent": "agent-a",
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    rc, out, err = _capture(
        [
            "plan",
            "--fixture",
            DEMO,
            "--landing-context",
            str(context),
            "--format",
            "json",
        ]
    )
    assert rc == 0, err
    obj = json.loads(out)
    node = next(item for item in obj["nodes"] if item["pr"] == 942)
    decision = next(item for item in obj["plan"]["per_pr_actions"] if item["pr"] == 942)
    assert node["assigned_agent"] == "agent-a"
    assert node["validation_evidence"] == "clean-validate-record"
    assert node["validation_authority"] == "hard-green"
    assert decision["action"] == "land-now"
    assert "no merge-gate wait" in decision["why"]


def test_plan_actions_has_capturable_summary_and_loud_lines() -> None:
    rc, out, _ = _capture(
        [
            "plan",
            "--fixture",
            DEMO,
            "--flaky-signatures",
            FLAKY,
            "--freshness-max-behind",
            "0",
            "--format",
            "actions",
        ]
    )
    assert rc == 0
    lines = out.splitlines()
    # Bare key=value summary lines (tick-hub `capture: true` lifts these).
    assert "land_now=1" in lines
    assert "outage=1" in lines
    assert any(line.startswith("stale_gates=") for line in lines)
    # Loud diagnostics + per-PR ACTION lines parseable by leading token.
    assert any(line.startswith("ERROR: ci-hosted-runner-outage-systemic") for line in lines)
    assert any(line.startswith("ACTION: land-now pr=1043") for line in lines)
    assert any(line.startswith("NOTE: evaluate-once-race pr=1050") for line in lines)


def test_graph_view() -> None:
    rc, out, _ = _capture(["graph", "--fixture", DEMO])
    assert rc == 0
    assert "Real conflicts" in out and "Stacks" in out
    rc, out, _ = _capture(["graph", "--fixture", DEMO, "--format", "json"])
    obj = json.loads(out)
    assert "conflict_edges" in obj and "plan" not in obj


_MECHANISM_FIXTURE = (
    "repo: OWNER/NAME\n"
    "base: integration\n"
    "prs:\n"
    "  - number: 1567\n"
    "    title: cancel-in-progress on the privileged workflow\n"
    "    head_ref: feat-priv\n"
    "    labels: [mechanism:cancel-in-progress]\n"
    "    changed_files: [.github/workflows/ci-privileged.yml]\n"
    "    checks:\n"
    "      - {name: CI, status: COMPLETED, conclusion: SUCCESS}\n"
    "      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}\n"
    "  - number: 1575\n"
    "    title: cancel-in-progress on the portable workflow\n"
    "    head_ref: feat-port\n"
    "    labels: [mechanism:cancel-in-progress]\n"
    "    changed_files: [.github/workflows/ci-portable.yml]\n"
    "    checks:\n"
    "      - {name: CI, status: COMPLETED, conclusion: SUCCESS}\n"
    "      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}\n"
)


def test_mechanism_overlap_surfaces_same_slug_without_file_conflict(tmp_path: Path) -> None:
    # #1567 and #1575 both declare mechanism:cancel-in-progress but edit DIFFERENT workflow files:
    # git sees no conflict and no file overlap, yet they change the same mechanism (the real
    # #1567-vs-#1575 near-miss). The semantic dimension must surface the pair anyway.
    fixture = tmp_path / "mechanism.yaml"
    fixture.write_text(_MECHANISM_FIXTURE)
    rc, out, _ = _capture(["plan", "--fixture", str(fixture), "--format", "json"])
    assert rc == 0
    obj = json.loads(out)
    assert obj["mechanism_overlap_edges"] == [
        {"a": 1567, "b": 1575, "mechanisms": ["cancel-in-progress"]}
    ]
    # Invisible to the file-based dimensions — that is exactly why the semantic edge is needed.
    assert obj["conflict_edges"] == []
    assert obj["file_overlap_edges"] == []


def test_mechanism_overlap_is_loud_in_actions_and_human(tmp_path: Path) -> None:
    fixture = tmp_path / "mechanism.yaml"
    fixture.write_text(_MECHANISM_FIXTURE)
    rc, actions, _ = _capture(["plan", "--fixture", str(fixture), "--format", "actions"])
    assert rc == 0
    lines = actions.splitlines()
    assert "mechanism_overlaps=1" in lines
    assert any(
        line.startswith("NOTE: mechanism-overlap prs=1567,1575") for line in lines
    )
    rc, human, _ = _capture(["plan", "--fixture", str(fixture)])
    assert rc == 0
    assert "Mechanism overlaps" in human
    assert "#1567 <-> #1575: cancel-in-progress" in human


def test_no_mechanism_overlap_when_slugs_differ(tmp_path: Path) -> None:
    # Distinct slugs must NOT be paired; only a shared mechanism links two PRs.
    fixture = tmp_path / "distinct.yaml"
    fixture.write_text(
        _MECHANISM_FIXTURE.replace(
            "labels: [mechanism:cancel-in-progress]\n"
            "    changed_files: [.github/workflows/ci-portable.yml]",
            "labels: [mechanism:CI_DAG_JOBS]\n"
            "    changed_files: [.github/workflows/ci-portable.yml]",
        )
    )
    rc, out, _ = _capture(["plan", "--fixture", str(fixture), "--format", "json"])
    assert rc == 0
    assert json.loads(out)["mechanism_overlap_edges"] == []


# A PR declares no mechanism: label at all; the mechanism candidate arrives ONLY as a diff-derived
# symbol, under a DIFFERENT SPELLING in each PR, in DIFFERENT files. The three-stage pipeline must
# still cluster them into ONE bucket by NORMALISING to the same enum value — the collision that raw
# per-string clustering (and every file-based dimension) misses.
_MECHANISM_SYMBOLS_FIXTURE = (
    "repo: OWNER/NAME\n"
    "base: integration\n"
    "prs:\n"
    "  - number: 2001\n"
    "    title: guard concurrency in the workflow\n"
    "    head_ref: feat-workflow\n"
    "    mechanism_symbols: [concurrency.cancel-in-progress]\n"
    "    changed_files: [.github/workflows/ci-a.yml]\n"
    "    checks:\n"
    "      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}\n"
    "  - number: 2002\n"
    "    title: add a cancel env var to the runner\n"
    "    head_ref: feat-runner\n"
    "    mechanism_symbols: [CANCEL_IN_PROGRESS]\n"
    "    changed_files: [rs/src/runner.rs]\n"
    "    checks:\n"
    "      - {name: merge-gate, status: COMPLETED, conclusion: SUCCESS}\n"
)


def test_mechanism_overlap_clusters_different_spellings_in_different_files(tmp_path: Path) -> None:
    # THE MONEY TEST for the enum redesign: no shared label, no shared file, and the two mechanism
    # candidates are spelled differently (concurrency.cancel-in-progress vs CANCEL_IN_PROGRESS).
    # CLASSIFY normalises both to Mechanism.CANCEL_IN_PROGRESS, so CLUSTER lands them in one edge.
    fixture = tmp_path / "symbols.yaml"
    fixture.write_text(_MECHANISM_SYMBOLS_FIXTURE)
    rc, out, _ = _capture(["plan", "--fixture", str(fixture), "--format", "json"])
    assert rc == 0
    obj = json.loads(out)
    assert obj["mechanism_overlap_edges"] == [
        {"a": 2001, "b": 2002, "mechanisms": ["cancel-in-progress"]}
    ]
    # Different spellings normalised to the same enum: nothing was left UNCLASSIFIED here.
    assert obj["unclassified_mechanism_candidates"] == []
    # Invisible to git: the pair shares no file and does not conflict.
    assert obj["conflict_edges"] == []
    assert obj["file_overlap_edges"] == []


def test_unclassified_mechanism_candidate_is_surfaced_loudly(tmp_path: Path) -> None:
    # A derived symbol that maps to NO enum member must be surfaced (not silently dropped): it is the
    # signal that the enum may need a new member. It must NOT create a mechanism edge.
    fixture = tmp_path / "unclassified.yaml"
    fixture.write_text(
        _MECHANISM_SYMBOLS_FIXTURE.replace(
            "mechanism_symbols: [CANCEL_IN_PROGRESS]",
            "mechanism_symbols: [SOME_BRAND_NEW_FLAG]",
        )
    )
    rc, out, _ = _capture(["plan", "--fixture", str(fixture), "--format", "json"])
    assert rc == 0
    obj = json.loads(out)
    # #2001 still classifies; #2002's SOME_BRAND_NEW_FLAG does not -> no shared mechanism, no edge.
    assert obj["mechanism_overlap_edges"] == []
    assert {"pr": 2002, "candidates": ["SOME_BRAND_NEW_FLAG"]} in obj[
        "unclassified_mechanism_candidates"
    ]
    # Loud in the actions view too.
    rc, actions, _ = _capture(["plan", "--fixture", str(fixture), "--format", "actions"])
    assert rc == 0
    lines = actions.splitlines()
    assert "unclassified_mechanisms=1" in lines
    assert any(
        line.startswith("NOTE: unclassified-mechanism pr=2002") for line in lines
    )


def test_classify_recognises_spellings_and_returns_none_for_unknown() -> None:
    from pr_landing_planner.mechanism import Mechanism, classify

    # Recognition across spellings for one mechanism.
    assert classify("CANCEL_IN_PROGRESS") is Mechanism.CANCEL_IN_PROGRESS
    assert classify("concurrency.cancel-in-progress") is Mechanism.CANCEL_IN_PROGRESS
    assert classify("cancel_in_progress") is Mechanism.CANCEL_IN_PROGRESS
    # Other seeded members via their aliases.
    assert classify("CI_DAG_JOBS") is Mechanism.DAG_SCHEDULER_WIDTH
    assert classify("merge-gate") is Mechanism.MERGE_GATE_REQUIRED_CHECKS
    # UNCLASSIFIED is a valid, load-bearing output.
    assert classify("SOME_BRAND_NEW_FLAG") is None
    assert classify("") is None
    # Boundary-aware: a bare token must not match a longer unrelated identifier it is a substring of.
    assert classify("dag-jobsworth") is None


def test_derive_symbols_from_diff_pulls_consts_and_yaml_keys() -> None:
    from pr_landing_planner.mechanism import derive_symbols_from_diff

    diff = (
        "--- a/x\n"
        "+++ b/x\n"
        "+CANCEL_IN_PROGRESS = true\n"
        "+  cancel-in-progress: true\n"
        "+SOME_NEW_FLAG=1\n"
        "-REMOVED_CONST = 0\n"
        " CONTEXT_CONST = 2\n"
    )
    symbols = derive_symbols_from_diff(diff)
    # Added lines only: SCREAMING_SNAKE consts + YAML-ish keys; removed/context lines ignored.
    assert symbols == ("CANCEL_IN_PROGRESS", "SOME_NEW_FLAG", "cancel-in-progress")


def test_status_view_and_threshold_warning() -> None:
    rc, out, _ = _capture(["status", "--fixture", DEMO, "--warn-threshold", "3"])
    assert rc == 0
    assert "Open PR health" in out
    assert "WARNING" in out  # 9 open PRs exceeds 3
    rc, out, _ = _capture(["status", "--fixture", DEMO, "--format", "json"])
    obj = json.loads(out)
    assert obj["summary"]["open"] == 9


def test_missing_fixture_exits_2() -> None:
    rc, _, err = _capture(["plan", "--fixture", "/nonexistent/nope.yaml"])
    assert rc == 2
    assert PROG in err


def test_version_via_module() -> None:
    py_root = Path(__file__).resolve().parent.parent
    result = subprocess.run(
        [sys.executable, "-m", "pr_landing_planner", "--version"],
        capture_output=True,
        text=True,
        check=True,
        cwd=str(py_root),
    )
    assert result.stdout.strip() == f"{PROG} {__version__}"
