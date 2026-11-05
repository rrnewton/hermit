"""Claude CLI backend contracts, using a local fake and no model calls."""

from __future__ import annotations

from dataclasses import replace
import json
from pathlib import Path

import pytest

from wrkviz import cli as timeline_cli
from wrkviz.naming import AgentNameError, AgentNameJob, name_agents
from wrkviz.summarize import SummaryError, SummaryJob, summarize_jobs
from wrkviz.token_usage import TokenUsage, parse_claude_json_usage


def _summary_job() -> SummaryJob:
    return SummaryJob(
        key="phase-one",
        team_slug="test-team",
        agent_label="transcript indexer",
        start_ms=1_800_000_000_000,
        end_ms=1_800_000_060_000,
        prior_context="The user asked for a durable transcript index.",
        transcript="Implemented the transcript index and passed 12 parser tests.",
        glossary="transcript index: chronological owner-intent records",
        stats={"messages": 2, "tool_calls": 1},
    )


def _name_job() -> AgentNameJob:
    return AgentNameJob(
        key="name-one",
        team_slug="test-team",
        thread_id="thread-one",
        start_ms=1_800_000_000_000,
        end_ms=1_800_000_060_000,
        official_path="/root/transcript_index_audit",
        coordinator_nickname="Noether",
        role="reviewer",
        depth=1,
        parent_official_path="/root",
        prior_context="The user called this the transcript index audit.",
        work_summary="The agent audited the transcript index and its parser tests.",
    )


def _write_fake_claude(path: Path) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
assert args[0] == "--print"
assert args[args.index("--output-format") + 1] == "json"
schema = json.loads(args[args.index("--json-schema") + 1])
assert "$schema" not in schema
assert args[args.index("--model") + 1] == "claude-test"
assert args[args.index("--effort") + 1] == "medium"
assert "--safe-mode" in args
assert "--no-session-persistence" in args
assert args[args.index("--permission-mode") + 1] == "dontAsk"
assert args[args.index("--tools") + 1] == ""
prompt = sys.stdin.read()
if "BEGIN_AGENT_NAME_JOBS_JSON\\n" in prompt:
    payload = prompt.split("BEGIN_AGENT_NAME_JOBS_JSON\\n", 1)[1].split(
        "END_AGENT_NAME_JOBS_JSON", 1
    )[0]
    jobs = json.loads(payload)
    assert schema["properties"]["names"]["type"] == "array"
    structured = {"names": [{
        "key": job["key"],
        "thread_id": job["thread_id"],
        "short_name": "Transcript index audit",
        "rationale": "The completed work audited the transcript index.",
        "lifetime_summary": "The agent audited the transcript index and verified its parser tests.",
    } for job in jobs]}
else:
    payload = prompt.split("BEGIN_JOBS_JSON\\n", 1)[1].split(
        "END_JOBS_JSON", 1
    )[0]
    jobs = json.loads(payload)
    assert schema["properties"]["summaries"]["type"] == "array"
    structured = {"summaries": [{
        "key": job["key"],
        "phrase": "Built transcript index",
        "paragraph": "Implemented a durable transcript index and passed its parser tests.",
        "work_summary": [{
            "at_ms": job["start_ms"],
            "text": "Implemented the transcript index and passed 12 parser tests.",
        }],
    } for job in jobs]}
result = {
    "type": "result",
    "subtype": "success",
    "is_error": False,
    "result": json.dumps(structured),
    "usage": {
        "input_tokens": 11,
        "cache_creation_input_tokens": 5,
        "cache_read_input_tokens": 7,
        "output_tokens": 13,
    },
}
mode = os.environ.get("FAKE_CLAUDE_MODE", "success")
if mode == "path-name":
    structured["names"][-1]["short_name"] = jobs[-1]["official_path"]
if mode in {"success", "path-name"}:
    result["structured_output"] = structured
elif mode == "failure":
    result["subtype"] = "error"
    result["is_error"] = True
    result["result"] = "provider deployment unavailable"
with Path(os.environ["FAKE_CLAUDE_LOG"]).open("a", encoding="utf-8") as handle:
    handle.write(json.dumps({"args": args, "cwd": str(Path.cwd())}) + "\\n")
print(json.dumps(result))
if mode == "failure":
    raise SystemExit(9)
""",
        encoding="utf-8",
    )
    path.chmod(0o755)


def test_claude_usage_normalizes_native_cache_counters() -> None:
    usage = parse_claude_json_usage(
        json.dumps(
            {
                "usage": {
                    "input_tokens": 11,
                    "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 7,
                    "output_tokens": 13,
                }
            }
        )
    )
    assert usage == TokenUsage(
        input_tokens=23,
        cached_input_tokens=7,
        cache_write_input_tokens=5,
        output_tokens=13,
    )
    assert usage.total_tokens == 36


def test_claude_summary_uses_safe_structured_mode_and_exact_receipts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake-claude"
    log = tmp_path / "claude-calls.jsonl"
    _write_fake_claude(fake)
    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(log))
    cache = tmp_path / "cache"

    results, stats = summarize_jobs(
        [_summary_job()],
        cache,
        backend="claude",
        model="claude-test",
        reasoning_effort="medium",
        max_workers=1,
        claude_command=(str(fake),),
    )

    assert results["phase-one"].phrase == "Built transcript index"
    assert stats.newly_spent_usage == TokenUsage(
        input_tokens=23,
        cached_input_tokens=7,
        cache_write_input_tokens=5,
        output_tokens=13,
    )
    assert stats.newly_spent_unknown_receipts == 0
    assert stats.usage_run_path is not None
    run = json.loads(stats.usage_run_path.read_text(encoding="utf-8"))
    assert run["backend"] == "claude"
    assert run["service_tier"] is None
    assert run["newly_spent_usage"]["total_tokens"] == 36
    call = json.loads(log.read_text(encoding="utf-8"))
    assert Path(call["cwd"]).name.startswith("wrkviz-summary-")
    args = call["args"]
    assert args[:3] == ["--print", "--output-format", "json"]
    serialized_schema = json.loads(args[args.index("--json-schema") + 1])
    assert "$schema" not in serialized_schema
    assert serialized_schema["properties"]["summaries"]["type"] == "array"
    assert args[args.index("--model") + 1] == "claude-test"
    assert args[args.index("--effort") + 1] == "medium"

    _, cached = summarize_jobs(
        [_summary_job()],
        cache,
        backend="claude",
        model="claude-test",
        reasoning_effort="medium",
        max_workers=1,
        claude_command=(str(fake),),
    )
    assert cached.hits == 1
    assert cached.newly_spent_usage == TokenUsage()
    assert len(log.read_text(encoding="utf-8").splitlines()) == 1


def test_claude_does_not_fall_back_to_result_string_and_keeps_usage(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake-claude"
    log = tmp_path / "claude-calls.jsonl"
    _write_fake_claude(fake)
    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(log))
    monkeypatch.setenv("FAKE_CLAUDE_MODE", "result-only")
    cache = tmp_path / "cache"

    with pytest.raises(SummaryError, match="malformed structured output"):
        summarize_jobs(
            [_summary_job()],
            cache,
            backend="claude",
            model="claude-test",
            reasoning_effort="medium",
            max_workers=1,
            claude_command=(str(fake),),
        )

    receipts = list((cache / "_usage" / "receipts").glob("*.json"))
    assert len(receipts) == 1
    receipt = json.loads(receipts[0].read_text(encoding="utf-8"))
    assert receipt["status"] == "failed"
    assert receipt["usage"]["total_tokens"] == 36
    assert not list(cache.glob("*.json"))
    assert not list((cache / "_usage" / "backend_outputs").glob("*.json"))


def test_claude_provider_failure_and_naming_are_accounted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake-claude"
    log = tmp_path / "claude-calls.jsonl"
    _write_fake_claude(fake)
    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(log))
    cache = tmp_path / "failed-cache"
    monkeypatch.setenv("FAKE_CLAUDE_MODE", "failure")
    with pytest.raises(SummaryError, match="deployment unavailable"):
        summarize_jobs(
            [_summary_job()],
            cache,
            backend="claude",
            model="claude-test",
            reasoning_effort="medium",
            max_workers=1,
            claude_command=(str(fake),),
        )
    failed_receipt = json.loads(
        next((cache / "_usage" / "receipts").glob("*.json")).read_text(
            encoding="utf-8"
        )
    )
    assert failed_receipt["usage"]["total_tokens"] == 36

    monkeypatch.setenv("FAKE_CLAUDE_MODE", "success")
    named, stats = name_agents(
        [_name_job()],
        tmp_path / "name-cache",
        backend="claude",
        model="claude-test",
        reasoning_effort="medium",
        max_workers=1,
        claude_command=(str(fake),),
    )
    assert named["thread-one"].short_name == "Transcript index audit"
    assert named["thread-one"].lifetime_summary is not None
    assert stats.newly_spent_usage.total_tokens == 36


def test_claude_path_name_repair_keeps_the_whole_batch_and_audit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake-claude"
    log = tmp_path / "claude-calls.jsonl"
    _write_fake_claude(fake)
    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(log))
    monkeypatch.setenv("FAKE_CLAUDE_MODE", "path-name")
    cache = tmp_path / "name-cache"
    first = _name_job()
    second = replace(
        first,
        key="name-two",
        thread_id="thread-two",
        official_path="/root/release_receipt_audit",
        coordinator_nickname="Curie",
    )

    named, stats = name_agents(
        [first, second],
        cache,
        backend="claude",
        model="claude-test",
        reasoning_effort="medium",
        max_workers=1,
        batch_size=12,
        claude_command=(str(fake),),
    )

    assert named[first.thread_id].short_name == "Transcript index audit"
    assert named[second.thread_id].short_name == "Release receipt audit"
    assert stats.misses == 2 and stats.batches == 1
    assert stats.newly_spent_usage.total_tokens == 36
    assert len(list(cache.glob("*.json"))) == 2
    receipt = json.loads(
        next((cache / "_usage" / "receipts").glob("*.json")).read_text(
            encoding="utf-8"
        )
    )
    assert receipt["status"] == "completed"
    audit = json.loads(
        (
            cache
            / "_usage"
            / "backend_outputs"
            / f"{receipt['receipt_id']}.json"
        ).read_text(encoding="utf-8")
    )
    assert audit["status"] == "repaired"
    assert audit["repairs"] == [
        {
            "key": second.key,
            "index": 1,
            "field": "short_name",
            "action": "normalized-path-or-slug",
            "rejected": second.official_path,
            "replacement": "Release receipt audit",
        }
    ]
    assert "BEGIN_AGENT_NAME_JOBS_JSON" not in audit["raw_output"]

    reused, cached = name_agents(
        [first, second],
        cache,
        backend="claude",
        model="claude-test",
        reasoning_effort="medium",
        max_workers=1,
        batch_size=12,
        claude_command=(str(fake),),
    )
    assert reused == named
    assert cached.hits == 2 and cached.misses == 0 and cached.batches == 0
    assert cached.newly_spent_usage == TokenUsage()
    assert len(log.read_text(encoding="utf-8").splitlines()) == 1


def test_claude_rejects_codex_only_service_tier(tmp_path: Path) -> None:
    with pytest.raises(SummaryError, match="only supported by the codex backend"):
        summarize_jobs(
            [_summary_job()],
            tmp_path / "cache",
            backend="claude",
            model="claude-test",
            service_tier="priority",
        )

    with pytest.raises(AgentNameError, match="only supported by the codex backend"):
        name_agents(
            [_name_job()],
            tmp_path / "name-cache",
            backend="claude",
            model="claude-test",
            service_tier="priority",
        )


def test_cli_exposes_and_forwards_claude_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    namespace = timeline_cli._parser().parse_args(
        [
            "summarize",
            "--output",
            str(tmp_path),
            "--team",
            "test-team",
            "--backend",
            "claude",
            "--model",
            "sonnet",
            "--reasoning-effort",
            "medium",
            "--claude-command",
            "/opt/test/claude-wrapper",
        ]
    )
    captured_args: list[tuple[object, ...]] = []
    captured_kwargs: list[dict[str, object]] = []

    class ExpectedCall(RuntimeError):
        pass

    def fake_summarize_archive(*args: object, **kwargs: object) -> None:
        captured_args.append(args)
        captured_kwargs.append(kwargs)
        raise ExpectedCall

    monkeypatch.setattr(timeline_cli, "summarize_archive", fake_summarize_archive)
    with pytest.raises(ExpectedCall):
        timeline_cli._summary_call(namespace)
    assert captured_args[0][2:4] == ("claude", "sonnet")
    assert captured_kwargs[0]["claude_command"] == ("/opt/test/claude-wrapper",)
    assert captured_kwargs[0]["reasoning_effort"] == "medium"


def test_cli_requires_explicit_claude_model(tmp_path: Path) -> None:
    namespace = timeline_cli._parser().parse_args(
        [
            "summarize",
            "--output",
            str(tmp_path),
            "--team",
            "test-team",
            "--backend",
            "claude",
        ]
    )

    with pytest.raises(ValueError, match="--model is required"):
        timeline_cli._summary_call(namespace)
