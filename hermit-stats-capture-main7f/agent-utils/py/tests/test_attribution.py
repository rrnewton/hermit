from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from dagrun.attribution import (
    DEFAULT_LOG_MAX_BYTES,
    TRUNCATION_MARKER,
    Culprit,
    ProcessObservation,
    RunEvidence,
    StepStream,
    _process_signature,
    bind_process_tests,
    log_max_bytes,
    recognize,
    require_step_end_ok,
)


def test_runnable_cpu_consumer_stays_cpu_burning_under_host_contention() -> None:
    assert _process_signature("R", 1.81, 0.83) == "cpu-burning"
    assert _process_signature("S", 1.81, 0.83) == "mixed-or-too-young"
    assert _process_signature("S", 1.81, 0.01) == "wall-stalled"


def test_recognizes_shared_harness_boundaries() -> None:
    start = recognize("test suite::hang ... ")
    end = recognize("test suite::ok ... ok")
    pytest_end = recognize("tests/test_x.py::test_y PASSED")
    tap_end = recognize("not ok 12 - rejects input")
    assert start is not None and (start.kind, start.name) == ("start", "suite::hang")
    assert end is not None and (end.kind, end.name, end.verdict) == (
        "end",
        "suite::ok",
        "ok",
    )
    assert pytest_end is not None and pytest_end.kind == "end"
    assert tap_end is not None and tap_end.verdict == "not ok"


def test_unterminated_tail_names_the_culprit() -> None:
    stream = StepStream("tests.suite", None)
    stream.ingest(b"test suite::alpha ... ok\ntest suite::gamma ... ")
    culprit = stream.culprit()
    assert culprit.test == "suite::gamma"
    assert culprit.completed == 1
    assert culprit.last_completed == "suite::alpha"


def test_parallel_boundaries_report_all_live_tests_with_elapsed_time() -> None:
    stream = StepStream("tests.suite", None)
    stream.ingest(b"##TEST-START suite::older\n")
    stream.ingest(b"##TEST-START suite::newer\n")
    culprit = stream.culprit()
    assert culprit.test == "suite::older"
    assert [test.name for test in culprit.in_flight] == ["suite::older", "suite::newer"]
    assert all(test.elapsed_s >= 0 for test in culprit.in_flight)
    assert "likely culprit test suite::older" in culprit.describe()


def test_process_binding_uses_exact_test_identity_and_refuses_unbound_rows() -> None:
    unknown = Culprit(None, "no boundaries", 0, None)
    unbound = ProcessObservation(10, 1, "shared-test-binary", 3.0, 0.0, "wall-stalled")
    assert bind_process_tests(unknown, (unbound,)).test is None

    bound = ProcessObservation(
        11,
        1,
        "test-bin --exact suite::hang",
        3.0,
        0.0,
        "wall-stalled",
        "suite::hang",
        "libtest --exact process argv",
    )
    culprit = bind_process_tests(unknown, (unbound, bound))
    assert culprit.test == "suite::hang"
    assert culprit.in_flight[0].basis == "libtest --exact process argv"


def test_evidence_is_private_and_records_boundaries(tmp_path: Path) -> None:
    directory = tmp_path / "evidence"
    evidence = RunEvidence.open(directory)
    assert evidence is not None
    stream = StepStream("g:j", evidence)
    stream.ingest(b"##TEST-START case\n##TEST-END case PASSED\n")
    evidence.record("done", [("ok", "true")])

    assert directory.stat().st_mode & 0o777 == 0o700
    assert (directory / "journal.jsonl").stat().st_mode & 0o777 == 0o600
    assert (directory / "g~3aj.log").stat().st_mode & 0o777 == 0o600
    records = [json.loads(line) for line in (directory / "journal.jsonl").read_text().splitlines()]
    assert [record["event"] for record in records] == ["test_start", "test_end", "done"]


def test_step_end_reader_refuses_string_verdicts() -> None:
    assert require_step_end_ok({"event": "step_end", "ok": False}) is False
    assert require_step_end_ok({"event": "step_end", "ok": True}) is True
    for malformed in ("false", "true", 0, 1, None):
        with pytest.raises(ValueError, match="ok must be a boolean"):
            require_step_end_ok({"event": "step_end", "ok": malformed})


def test_evidence_refuses_symlinks_and_fifos_without_blocking(tmp_path: Path) -> None:
    target = tmp_path / "target"
    target.mkdir(mode=0o700)
    link = tmp_path / "link"
    link.symlink_to(target, target_is_directory=True)
    assert RunEvidence.open(link) is None

    journal_dir = tmp_path / "journal-fifo"
    journal_dir.mkdir(mode=0o700)
    os.mkfifo(journal_dir / "journal.jsonl", mode=0o600)
    assert RunEvidence.open(journal_dir) is None

    step_dir = tmp_path / "step-fifo"
    evidence = RunEvidence.open(step_dir)
    assert evidence is not None
    os.mkfifo(step_dir / "g~3aj.log", mode=0o600)
    assert evidence.open_step_log("g:j") is None


def test_evidence_rejects_overlong_log_name(tmp_path: Path) -> None:
    evidence = RunEvidence.open(tmp_path / "evidence")
    assert evidence is not None
    assert evidence.open_step_log("/" * 100) is None


def test_step_log_is_bounded_and_says_so(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A capped step log must be truncated EXACTLY, marked, and journaled.

    The marker and journal record are the point. A log that stops early without saying so is
    a silently incomplete evidence file, and a later reader cannot distinguish "the step
    printed nothing more" from "we stopped writing" -- which is worse than no evidence at all.
    """
    monkeypatch.setenv("DAGRUN_LOG_MAX_BYTES", "100")
    directory = tmp_path / "evidence"
    evidence = RunEvidence.open(directory)
    assert evidence is not None
    stream = StepStream("g.j", evidence)
    stream.ingest(b"A" * 5000)

    log = (directory / "g.j.log").read_bytes()
    # EXACT: 100 payload bytes, not "the chunk that crossed 100". A ceiling that depends on
    # the reader's buffer size would differ between the two engines and break `make cross`.
    assert log[:100] == b"A" * 100
    assert log == b"A" * 100 + TRUNCATION_MARKER.encode()
    events = [
        json.loads(line)
        for line in (directory / "journal.jsonl").read_text().splitlines()
    ]
    truncations = [e for e in events if e["event"] == "step_log_truncated"]
    assert len(truncations) == 1
    assert truncations[0]["step"] == "g.j" and truncations[0]["limit_bytes"] == "100"

    # Announced ONCE, however many further chunks arrive.
    stream.ingest(b"B" * 5000)
    assert (directory / "g.j.log").read_bytes() == log
    events = [
        json.loads(line)
        for line in (directory / "journal.jsonl").read_text().splitlines()
    ]
    assert len([e for e in events if e["event"] == "step_log_truncated"]) == 1


def test_classification_survives_a_truncated_log(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Bounding the DISK must not cost the attribution the disk was there to support."""
    monkeypatch.setenv("DAGRUN_LOG_MAX_BYTES", "10")
    directory = tmp_path / "evidence"
    evidence = RunEvidence.open(directory)
    assert evidence is not None
    stream = StepStream("g.j", evidence)
    stream.ingest(b"A" * 500 + b"\n")
    stream.ingest(b"test mymod::mytest ... ")

    events = [
        json.loads(line)
        for line in (directory / "journal.jsonl").read_text().splitlines()
    ]
    kinds = [e["event"] for e in events]
    assert "step_log_truncated" in kinds
    starts = [e for e in events if e["event"] == "test_start"]
    assert [e["test"] for e in starts] == ["mymod::mytest"], (
        "the hung test must still be named after the log stopped being written"
    )


def test_log_ceiling_env_parsing() -> None:
    """`0` means unlimited; garbage is REPORTED and falls back, never silently unlimited."""
    saved = os.environ.pop("DAGRUN_LOG_MAX_BYTES", None)
    try:
        assert log_max_bytes() == DEFAULT_LOG_MAX_BYTES
        os.environ["DAGRUN_LOG_MAX_BYTES"] = "0"
        assert log_max_bytes() is None
        os.environ["DAGRUN_LOG_MAX_BYTES"] = "4096"
        assert log_max_bytes() == 4096
        os.environ["DAGRUN_LOG_MAX_BYTES"] = "  8192  "
        assert log_max_bytes() == 8192
        for bad in ("banana", "-1", "1.5"):
            os.environ["DAGRUN_LOG_MAX_BYTES"] = bad
            assert log_max_bytes() == DEFAULT_LOG_MAX_BYTES, f"{bad} must fall back, not unlimit"
    finally:
        os.environ.pop("DAGRUN_LOG_MAX_BYTES", None)
        if saved is not None:
            os.environ["DAGRUN_LOG_MAX_BYTES"] = saved
