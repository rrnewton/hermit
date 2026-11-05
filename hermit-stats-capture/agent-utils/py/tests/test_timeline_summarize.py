"""Tests for the content-addressed timeline summarization layer."""

from __future__ import annotations

import hashlib
import json
import sys
from collections.abc import Mapping
from dataclasses import replace
from pathlib import Path

import pytest

from wrkviz.summarize import (
    GLOSSARY_DEFINITION_PROMPT_VERSION,
    GLOSSARY_DEFINITION_STYLE,
    PLAIN_LANGUAGE_ROLLUP_STYLE,
    PROMPT_VERSION,
    PROJECT_OVERVIEW_PROMPT_VERSION,
    PROJECT_OVERVIEW_STYLE,
    SummaryError,
    SummaryJob,
    TECHNICAL_ROLLUP_STYLE,
    _PendingJob,
    _input_hash,
    _legacy_input_hash,
    _parse_backend_output,
    build_summary_prompt,
    clean_summary_prose,
    knowledge_text_has_link,
    summarize_jobs,
)
from wrkviz.token_usage import TokenUsage


def _job(
    key: str,
    transcript: str = "Implemented transcript indexing. All 12 parser tests passed.",
    stats: Mapping[str, int] | None = None,
) -> SummaryJob:
    return SummaryJob(
        key=key,
        team_slug="codex-widget",
        agent_label="coordinator" if key == "root" else key,
        start_ms=1_800_000_000_000,
        end_ms=1_800_000_060_000,
        prior_context="The user named this workstream transcript-DAG and then spawned the agent.",
        transcript=transcript,
        glossary="transcript-DAG: the durable multi-agent history and summary archive",
        stats=dict(stats or {"messages": 4, "tool_calls": 2}),
    )


def test_fingerprint_is_canonical_and_second_run_is_a_no_churn_hit(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    first_job = _job("root", stats={"messages": 4, "tool_calls": 2})
    reordered_stats = _job("root", stats={"tool_calls": 2, "messages": 4})

    first, first_stats = summarize_jobs(
        [first_job], cache, backend="heuristic", model="offline"
    )
    assert first_stats.hits == 0
    assert first_stats.misses == 1
    assert first_stats.batches == 1
    assert "Implemented transcript indexing" in first["root"].paragraph
    assert len(first["root"].phrase) <= 80

    cache_file = next(cache.glob("*.json"))
    cache_json = json.loads(cache_file.read_text(encoding="utf-8"))
    assert cache_json["format"] == "agent-team-timeline-model-artifact"
    assert cache_json["artifact"]["model"] == "offline"
    assert cache_json["artifact"]["context_coverage"]["known"] is True
    original_bytes = cache_file.read_bytes()
    original_mtime = cache_file.stat().st_mtime_ns

    second, second_stats = summarize_jobs(
        [reordered_stats], cache, backend="heuristic", model="offline"
    )
    assert second == first
    assert second_stats.hits == 1
    assert second_stats.misses == 0
    assert second_stats.batches == 0
    assert second_stats.cache_hits == 1
    assert second_stats.backend_batches == 0
    assert cache_file.read_bytes() == original_bytes
    assert cache_file.stat().st_mtime_ns == original_mtime


def test_summary_prose_omits_transcript_timestamps_roles_tools_and_encrypted_spawn() -> None:
    noisy = (
        "## [2026-08-05T04:41:50.424000Z] AGENT: [Encrypted Codex collaboration "
        "new task from /root to /root/audit; message body unavailable offline.] "
        "[2026-08-05T04:41:53.546000Z] ASSISTANT: Implemented the parser fix. "
        "[2026-08-05T04:41:55Z] TOOLS: 2 bash, 1 git "
        "[1785904916000] Parser audit: All 12 regression tests passed."
    )

    assert clean_summary_prose(noisy) == (
        "Implemented the parser fix. All 12 regression tests passed."
    )


def test_tool_scaffolding_cleanup_preserves_following_substantive_prose() -> None:
    noisy = (
        "[2026-08-05T04:41:55Z] TOOLS: 2 bash. "
        "Shipped the parser fix and all 12 tests passed."
    )

    assert clean_summary_prose(noisy) == (
        "Shipped the parser fix and all 12 tests passed."
    )


def test_heuristic_summary_never_promotes_transcript_scaffolding(tmp_path: Path) -> None:
    job = _job(
        "agent-a",
        "[2026-08-05T04:41:50.424000Z] AGENT: [Encrypted Codex collaboration new task "
        "from /root to /root/audit; message body unavailable offline.]\n\n"
        "[2026-08-05T04:41:53.546000Z] ASSISTANT: Implemented the parser fix.\n\n"
        "[2026-08-05T04:41:55Z] TOOLS: 2 bash, 1 git",
    )

    results, _ = summarize_jobs(
        [job], tmp_path / "cache", backend="heuristic", model="offline"
    )
    rendered = " ".join(
        [results[job.key].phrase, results[job.key].paragraph]
        + [item.text for item in results[job.key].work_summary]
    )
    assert "2026-08-05" not in rendered
    assert "AGENT:" not in rendered
    assert "ASSISTANT:" not in rendered
    assert "TOOLS:" not in rendered
    assert "Encrypted Codex" not in rendered
    assert "Implemented the parser fix" in rendered


def test_clean_runs_have_deterministic_fingerprints(tmp_path: Path) -> None:
    job = _job("agent-a")
    first, _ = summarize_jobs(
        [job], tmp_path / "one", backend="heuristic", model="offline"
    )
    second, _ = summarize_jobs(
        [job], tmp_path / "two", backend="heuristic", model="offline"
    )
    assert first[job.key].input_hash == second[job.key].input_hash
    assert first[job.key].phrase == second[job.key].phrase
    assert first[job.key].paragraph == second[job.key].paragraph
    assert first[job.key].work_summary == second[job.key].work_summary
    assert _input_hash(job, "codex", "same-model", "high") != _input_hash(
        job, "codex", "same-model", "xhigh"
    )


def test_changing_one_job_invalidates_only_that_job(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    jobs = [_job("agent-a"), _job("agent-b")]
    first, _ = summarize_jobs(jobs, cache, backend="heuristic", model="offline")

    changed = replace(
        jobs[1], transcript="Fixed the scheduler race. The 37 stress tests passed."
    )
    second, stats = summarize_jobs(
        [jobs[0], changed], cache, backend="heuristic", model="offline"
    )
    assert stats.hits == 1
    assert stats.misses == 1
    assert stats.batches == 1
    assert second["agent-a"] == first["agent-a"]
    assert second["agent-b"].input_hash != first["agent-b"].input_hash
    assert "scheduler race" in second["agent-b"].paragraph
    assert len(list(cache.glob("*.json"))) == 3


def test_prompt_contains_full_context_glossary_and_terminology_rules() -> None:
    job = _job("agent-a")
    prompt = build_summary_prompt([job])
    assert job.prior_context in prompt
    assert job.transcript in prompt
    assert job.glossary in prompt
    assert '"messages": 4' in prompt
    assert "cross the subagent spawn edge" in prompt
    assert "phase 2" in prompt and "descriptive workstream" in prompt
    assert "Do not call tools" in prompt
    assert PROMPT_VERSION in prompt


def test_rollup_prompts_have_distinct_content_led_audience_contracts() -> None:
    technical = replace(_job("technical"), summary_style=TECHNICAL_ROLLUP_STYLE)
    plain = replace(
        _job("plain"),
        summary_style=PLAIN_LANGUAGE_ROLLUP_STYLE,
        factual_context=(
            "Authoritative technical daily summary: the fix was validated but still "
            "awaited review; it did not land."
        ),
    )

    technical_prompt = build_summary_prompt([technical])
    plain_prompt = build_summary_prompt([plain])

    assert "technical summary" in technical_prompt
    assert "opaque referent" in technical_prompt
    assert "interested newcomer" in plain_prompt
    assert "what the project or product is" in plain_prompt
    assert plain.factual_context in plain_prompt
    assert "Never upgrade pending, approved, validated" in plain_prompt
    assert "do not invent Markdown links or glossary entries" in plain_prompt
    assert _input_hash(technical, "codex", "same-model") != _input_hash(
        plain, "codex", "same-model"
    )
    assert "agent-team-timeline-plain-rollup-v4" in plain_prompt


def test_project_overview_is_evidence_bounded_and_legacy_definitions_are_disabled() -> None:
    overview = replace(_job("overview"), summary_style=PROJECT_OVERVIEW_STYLE)
    definition = replace(
        _job("definition"), summary_style=GLOSSARY_DEFINITION_STYLE
    )

    overview_prompt = build_summary_prompt([overview])
    assert PROJECT_OVERVIEW_PROMPT_VERSION in overview_prompt
    assert "durable project overview" in overview_prompt
    assert "do not infer facts" in overview_prompt
    assert "Never emit a URL, Markdown link" in overview_prompt
    assert GLOSSARY_DEFINITION_PROMPT_VERSION.endswith("glossary-definition-v2")
    assert _input_hash(overview, "codex", "same-model") != _input_hash(
        definition, "codex", "same-model"
    )
    with pytest.raises(SummaryError, match="unsupported style 'glossary-definition'"):
        build_summary_prompt([definition])


def test_heuristic_knowledge_results_are_honest_context_only(
    tmp_path: Path,
) -> None:
    job = replace(_job("knowledge"), summary_style=PROJECT_OVERVIEW_STYLE)

    results, stats = summarize_jobs(
        [job], tmp_path / "cache", backend="heuristic", model="offline"
    )

    result = results[job.key]
    assert result.phrase == "Insufficient evidence"
    assert result.paragraph.startswith("Insufficient evidence:")
    assert "model-backed knowledge pass" in result.paragraph
    assert result.work_summary == ()
    assert stats.newly_spent_usage == TokenUsage()


@pytest.mark.parametrize(
    "value",
    (
        "See [project docs](https://example.invalid).",
        "See https://example.invalid/docs.",
        "See example.com/docs.",
        "See #glossary/term-invented.",
        "![diagram](relative.png)",
    ),
)
def test_generated_knowledge_rejects_unverified_links(value: str) -> None:
    assert knowledge_text_has_link(value)


def test_generated_knowledge_allows_plain_project_identifiers() -> None:
    assert not knowledge_text_has_link(
        "Node.js and exact-head are project terms; pull request #123 is supplementary evidence."
    )


def test_backend_cannot_persist_linked_generated_knowledge(tmp_path: Path) -> None:
    job = replace(_job("overview"), summary_style=PROJECT_OVERVIEW_STYLE)
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Project overview supported",
                "paragraph": "Read [invented docs](https://example.invalid) for details.",
                "work_summary": [],
            }
        ]
    }

    with pytest.raises(SummaryError, match="must not contain links or URLs"):
        _parse_backend_output(
            json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
        )


def test_one_backend_batch_cannot_mix_summary_audiences() -> None:
    technical = replace(_job("technical"), summary_style=TECHNICAL_ROLLUP_STYLE)
    plain = replace(
        _job("plain"),
        summary_style=PLAIN_LANGUAGE_ROLLUP_STYLE,
        factual_context="Authoritative same-period technical facts.",
    )

    with pytest.raises(SummaryError, match="cannot mix summary styles"):
        build_summary_prompt([technical, plain])


def test_plain_rollup_requires_same_period_technical_facts() -> None:
    plain = replace(_job("plain"), summary_style=PLAIN_LANGUAGE_ROLLUP_STYLE)

    with pytest.raises(SummaryError, match="lacks same-period technical facts"):
        build_summary_prompt([plain])


def test_service_tier_is_cache_identity_provenance() -> None:
    job = _job("agent-a")
    inherited = _input_hash(job, "codex", "gpt-test", "high")
    explicit_default = _input_hash(
        job, "codex", "gpt-test", "high", service_tier="default"
    )
    priority = _input_hash(
        job, "codex", "gpt-test", "high", service_tier="priority"
    )
    assert inherited == explicit_default
    assert inherited != priority
    assert priority != _input_hash(
        job, "codex", "gpt-test", "high", service_tier="standard"
    )


def test_blank_service_tier_is_rejected_before_backend_work(tmp_path: Path) -> None:
    with pytest.raises(SummaryError, match="service tier must not be empty"):
        summarize_jobs(
            [_job("agent-a")],
            tmp_path / "cache",
            backend="heuristic",
            model="offline",
            service_tier="  ",
        )


def test_service_tier_is_rejected_for_heuristic_backend(tmp_path: Path) -> None:
    with pytest.raises(
        SummaryError, match="service tier is only supported by the codex backend"
    ):
        summarize_jobs(
            [_job("agent-a")],
            tmp_path / "cache",
            backend="heuristic",
            model="offline",
            service_tier="priority",
        )


def _write_fake_codex(path: Path) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

args = sys.argv[1:]
assert args[0] == "exec"
for required in (
    "--ephemeral", "--skip-git-repo-check", "--sandbox", "read-only",
    "--model", "--json", "--output-schema", "--output-last-message", "-",
):
    assert required in args
expected_service_tier = os.environ.get("FAKE_CODEX_SERVICE_TIER")
if expected_service_tier is not None:
    assert f'service_tier="{expected_service_tier}"' in args
assert Path.cwd().name.startswith("wrkviz-summary-")
repository_check = subprocess.run(
    ["git", "rev-parse", "--is-inside-work-tree"],
    text=True,
    capture_output=True,
    check=True,
)
assert repository_check.stdout.strip() == "true"
compatibility_command = shutil.which("hg") or shutil.which("sl")
if compatibility_command is not None:
    compatibility_check = subprocess.run(
        [compatibility_command, "root"],
        text=True,
        capture_output=True,
        check=True,
    )
    assert Path(compatibility_check.stdout.strip()) == Path.cwd()
schema_path = Path(args[args.index("--output-schema") + 1])
schema = json.loads(schema_path.read_text(encoding="utf-8"))
assert schema["properties"]["summaries"]["type"] == "array"
prompt = sys.stdin.read()
payload_text = prompt.split("BEGIN_JOBS_JSON\\n", 1)[1].split("END_JOBS_JSON", 1)[0]
jobs = json.loads(payload_text)
summaries = []
for job in jobs:
    style = job.get("summary_style", "phase")
    if style == "project-overview":
        phrase = "Project overview supported"
        paragraph = "Widget runs guest software in a controlled, repeatable environment."
        work_summary = []
    elif style == "glossary-definition":
        phrase = "Definition supported"
        paragraph = "exact-head is a project check that keeps a release bound to one revision."
        work_summary = []
    else:
        phrase = "Built " + job["key"]
        paragraph = "Implemented durable summary output for " + job["agent_label"] + "."
        work_summary = [{
            "at_ms": job["start_ms"],
            "text": "Implemented durable summary output.",
        }]
    summaries.append({
        "key": job["key"],
        "phrase": phrase,
        "paragraph": paragraph,
        "work_summary": work_summary,
    })
output_path = Path(args[args.index("--output-last-message") + 1])
output_path.write_text(json.dumps({"summaries": summaries}), encoding="utf-8")
with Path(os.environ["FAKE_CODEX_LOG"]).open("a", encoding="utf-8") as handle:
    handle.write("CALL\\n")
    handle.write(prompt)
    handle.write("PROMPT_END\\n")
print(json.dumps({
    "type": "turn.completed",
    "usage": {
        "input_tokens": 100,
        "cached_input_tokens": 40,
        "cache_write_input_tokens": 5,
        "output_tokens": 20,
        "reasoning_output_tokens": 7,
    },
}))
""",
        encoding="utf-8",
    )


def test_codex_backend_batches_with_schema_stdin_and_temp_workdir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake_codex.py"
    log = tmp_path / "calls.log"
    _write_fake_codex(fake)
    monkeypatch.setenv("FAKE_CODEX_LOG", str(log))
    monkeypatch.setenv("FAKE_CODEX_SERVICE_TIER", "priority")
    jobs = [_job("agent-a"), _job("agent-b"), _job("agent-c")]

    results, stats = summarize_jobs(
        jobs,
        tmp_path / "cache",
        backend="codex",
        model="gpt-test",
        max_workers=1,
        batch_size=2,
        codex_command=(sys.executable, str(fake)),
        service_tier="priority",
    )
    assert stats.hits == 0
    assert stats.misses == 3
    assert stats.batches == 2
    assert stats.newly_spent_usage == TokenUsage(
        input_tokens=200,
        cached_input_tokens=80,
        cache_write_input_tokens=10,
        output_tokens=40,
        reasoning_output_tokens=14,
    )
    assert stats.newly_spent_usage.total_tokens == 240
    assert stats.newly_spent_unknown_receipts == 0
    assert stats.artifact_generation_usage == stats.newly_spent_usage
    assert stats.artifact_generation_unknown_receipts == 0
    assert stats.unknown_legacy_artifacts == 0
    assert stats.usage_run_path is not None
    run = json.loads(stats.usage_run_path.read_text(encoding="utf-8"))
    assert run["schema_version"] == 2
    assert run["model"] == "gpt-test"
    assert run["service_tier"] == "priority"
    assert run["newly_spent_usage"]["total_tokens"] == 240
    assert len(run["batch_receipt_ids"]) == 2
    assert list(results) == [job.key for job in jobs]
    assert results["agent-a"].phrase == "Built agent-a"
    assert results["agent-a"].work_summary[0].at_ms == jobs[0].start_ms
    log_text = log.read_text(encoding="utf-8")
    assert log_text.count("CALL\n") == 2
    assert jobs[0].prior_context in log_text
    assert jobs[0].glossary in log_text

    _, cached_stats = summarize_jobs(
        jobs,
        tmp_path / "cache",
        backend="codex",
        model="gpt-test",
        max_workers=1,
        batch_size=2,
        codex_command=(sys.executable, str(fake)),
        service_tier="priority",
    )
    assert cached_stats.hits == 3
    assert cached_stats.batches == 0
    assert cached_stats.newly_spent_usage == TokenUsage()
    assert cached_stats.newly_spent_unknown_receipts == 0
    assert cached_stats.artifact_generation_usage == stats.artifact_generation_usage
    assert cached_stats.artifact_generation_unknown_receipts == 0
    assert log.read_text(encoding="utf-8") == log_text

    monkeypatch.setenv("FAKE_CODEX_SERVICE_TIER", "default")
    _, default_stats = summarize_jobs(
        [jobs[0]],
        tmp_path / "default-cache",
        backend="codex",
        model="gpt-test",
        max_workers=1,
        batch_size=2,
        codex_command=(sys.executable, str(fake)),
    )
    assert default_stats.usage_run_path is not None
    default_run = json.loads(
        default_stats.usage_run_path.read_text(encoding="utf-8")
    )
    assert default_run["service_tier"] == "default"
    default_receipt_id = default_run["batch_receipt_ids"][0]
    default_receipt = json.loads(
        (
            default_stats.usage_run_path.parent.parent
            / "receipts"
            / f"{default_receipt_id}.json"
        ).read_text(encoding="utf-8")
    )
    assert default_receipt["service_tier"] == "default"


def test_codex_backend_caches_project_overview_and_rejects_legacy_definition(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake_codex.py"
    log = tmp_path / "calls.log"
    _write_fake_codex(fake)
    monkeypatch.setenv("FAKE_CODEX_LOG", str(log))
    overview = replace(_job("overview"), summary_style=PROJECT_OVERVIEW_STYLE)
    definition = replace(
        _job("definition"), summary_style=GLOSSARY_DEFINITION_STYLE
    )

    overview_results, overview_stats = summarize_jobs(
        [overview],
        tmp_path / "cache",
        backend="codex",
        model="gpt-test",
        max_workers=1,
        codex_command=(sys.executable, str(fake)),
    )
    assert overview_results["overview"].phrase == "Project overview supported"
    assert overview_stats.newly_spent_usage.total_tokens == 120
    with pytest.raises(SummaryError, match="unsupported style 'glossary-definition'"):
        summarize_jobs(
            [definition],
            tmp_path / "cache",
            backend="codex",
            model="gpt-test",
            max_workers=1,
            codex_command=(sys.executable, str(fake)),
        )
    assert log.read_text(encoding="utf-8").count("CALL\n") == 1

    _, cached = summarize_jobs(
        [overview],
        tmp_path / "cache",
        backend="codex",
        model="gpt-test",
        max_workers=1,
        codex_command=(sys.executable, str(fake)),
    )
    assert cached.hits == 1
    assert cached.newly_spent_usage == TokenUsage()


def test_historical_end_inclusive_paid_cache_remains_a_hit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cache = tmp_path / "cache"
    fake = tmp_path / "fake_codex.py"
    log = tmp_path / "calls.log"
    _write_fake_codex(fake)
    monkeypatch.setenv("FAKE_CODEX_LOG", str(log))
    job = _job("agent-a")

    generated, generated_stats = summarize_jobs(
        [job],
        cache,
        backend="codex",
        model="gpt-test",
        max_workers=1,
        codex_command=(sys.executable, str(fake)),
    )
    assert generated_stats.misses == 1
    assert log.read_text(encoding="utf-8").count("CALL\n") == 1

    cache_path = cache / f"{_input_hash(job, 'codex', 'gpt-test')}.json"
    historical = json.loads(cache_path.read_text(encoding="utf-8"))
    assert historical["result"]["work_summary"][0]["at_ms"] == job.start_ms
    historical["result"]["work_summary"][0]["at_ms"] = job.end_ms
    cache_path.write_text(json.dumps(historical), encoding="utf-8")
    historical_bytes = cache_path.read_bytes()

    reused, reused_stats = summarize_jobs(
        [job],
        cache,
        backend="codex",
        model="gpt-test",
        max_workers=1,
        codex_command=(sys.executable, str(fake)),
    )

    assert reused_stats.hits == 1
    assert reused_stats.misses == 0
    assert reused_stats.batches == 0
    assert reused_stats.newly_spent_usage == TokenUsage()
    assert reused[job.key].phrase == generated[job.key].phrase
    assert reused[job.key].work_summary[0].at_ms == job.end_ms
    assert cache_path.read_bytes() == historical_bytes
    assert log.read_text(encoding="utf-8").count("CALL\n") == 1


def test_corrupt_cache_is_ignored_and_regenerated(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    job = _job("agent-a")
    summarize_jobs([job], cache, backend="heuristic", model="offline")
    cache_file = next(cache.glob("*.json"))
    cache_file.write_text("{ definitely not JSON", encoding="utf-8")

    results, stats = summarize_jobs(
        [job], cache, backend="heuristic", model="offline"
    )
    assert stats.hits == 0
    assert stats.misses == 1
    assert "transcript indexing" in results[job.key].paragraph
    assert cache_file.read_text(encoding="utf-8").startswith("{")
    json.loads(cache_file.read_text(encoding="utf-8"))


def test_legacy_cache_is_a_hit_with_explicitly_unknown_historical_cost(
    tmp_path: Path,
) -> None:
    cache = tmp_path / "cache"
    job = _job("agent-a")
    original, _ = summarize_jobs([job], cache, backend="heuristic", model="offline")
    cache_file = next(cache.glob("*.json"))
    raw = json.loads(cache_file.read_text(encoding="utf-8"))
    legacy_hash = _legacy_input_hash(job, "heuristic", "offline")
    legacy_result = raw["result"]
    legacy_result["input_hash"] = legacy_hash
    cache_file.unlink()
    legacy_file = cache / f"{legacy_hash}.json"
    legacy_file.write_text(
        json.dumps(
            {"cache_version": 1, "backend": "heuristic", "result": legacy_result}
        ),
        encoding="utf-8",
    )

    reused, stats = summarize_jobs(
        [job], cache, backend="heuristic", model="offline"
    )
    assert reused[job.key].phrase == original[job.key].phrase
    provenance = reused[job.key].artifact_provenance
    assert provenance is not None
    assert provenance.legacy_storage is True
    assert provenance.context_coverage.known is False
    assert reused[job.key].input_hash == legacy_hash
    assert stats.hits == 1
    assert stats.misses == 0
    assert stats.newly_spent_usage == TokenUsage()
    assert stats.unknown_legacy_artifacts == 1


def test_backend_failure_is_concise_and_preserves_existing_cache(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    cache.mkdir()
    job = _job("agent-a")
    cache_file = cache / f"{_input_hash(job, 'codex', 'gpt-test')}.json"
    original = b"corrupt but pre-existing\n"
    cache_file.write_bytes(original)

    with pytest.raises(SummaryError, match="exit 7") as caught:
        summarize_jobs(
            [job],
            cache,
            backend="codex",
            model="gpt-test",
            codex_command=(
                sys.executable,
                "-c",
                (
                    "import json,sys; "
                    "print(json.dumps({'type':'turn.completed','usage':{"
                    "'input_tokens':31,'cached_input_tokens':11,'output_tokens':7}})); "
                    "sys.stderr.write('boom'); sys.exit(7)"
                ),
            ),
        )
    assert len(str(caught.value)) < 300
    assert "boom" not in str(caught.value)
    assert "failed usage receipt:" in str(caught.value)
    assert cache_file.read_bytes() == original
    receipt_paths = list((cache / "_usage" / "receipts").glob("*.json"))
    assert len(receipt_paths) == 1
    receipt = json.loads(receipt_paths[0].read_text(encoding="utf-8"))
    assert receipt["status"] == "failed"
    assert receipt["model"] == "gpt-test"
    assert receipt["usage"] == {
        "input_tokens": 31,
        "cached_input_tokens": 11,
        "cache_write_input_tokens": 0,
        "output_tokens": 7,
        "reasoning_output_tokens": 0,
        "total_tokens": 38,
    }


def test_backend_failure_does_not_persist_captured_streams(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    job = _job("agent-a")
    stdout_sentinel = "STDOUT-FAILURE-SENTINEL"
    stderr_sentinel = "STDERR-FAILURE-SENTINEL"
    raw_output = '{"safe":"backend last message"}'
    backend = f"""
import sys
from pathlib import Path

args = sys.argv[1:]
Path(args[args.index("--output-last-message") + 1]).write_text(
    {raw_output!r}, encoding="utf-8"
)
print("error: {stdout_sentinel}")
sys.stderr.write("{stderr_sentinel}")
raise SystemExit(7)
"""

    with pytest.raises(SummaryError, match="failed with exit 7") as caught:
        summarize_jobs(
            [job],
            cache,
            backend="codex",
            model="gpt-test",
            codex_command=(sys.executable, "-c", backend),
        )
    assert stdout_sentinel not in str(caught.value)
    assert stderr_sentinel not in str(caught.value)

    receipt_path = next((cache / "_usage" / "receipts").glob("*.json"))
    receipt_text = receipt_path.read_text(encoding="utf-8")
    receipt = json.loads(receipt_text)
    assert receipt["error"] == "codex summary batch failed with exit 7"
    assert stdout_sentinel not in receipt_text
    assert stderr_sentinel not in receipt_text

    audit_path = (
        cache / "_usage" / "backend_outputs" / f"{receipt['receipt_id']}.json"
    )
    audit_text = audit_path.read_text(encoding="utf-8")
    audit = json.loads(audit_text)
    assert set(audit) == {
        "schema_version",
        "receipt_id",
        "status",
        "raw_output_sha256",
        "raw_output",
        "jobs",
    }
    assert audit["status"] == "failed"
    assert audit["raw_output"] == raw_output
    assert audit["raw_output_sha256"] == hashlib.sha256(
        raw_output.encode("utf-8")
    ).hexdigest()
    assert audit["jobs"] == [
        {
            "key": job.key,
            "start_ms": job.start_ms,
            "end_ms": job.end_ms,
            "rejected_bullets": [],
        }
    ]
    assert stdout_sentinel not in audit_text
    assert stderr_sentinel not in audit_text


def test_invalid_model_output_preserves_terminal_usage_receipt(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    job = _job("agent-a")
    backend = """
import json
import sys
from pathlib import Path

args = sys.argv[1:]
output = Path(args[args.index("--output-last-message") + 1])
output.write_text("not valid structured JSON", encoding="utf-8")
print(json.dumps({
    "type": "turn.completed",
    "usage": {
        "input_tokens": 53,
        "cached_input_tokens": 17,
        "output_tokens": 9,
    },
}))
"""
    with pytest.raises(SummaryError, match="failed usage receipt:") as caught:
        summarize_jobs(
            [job],
            cache,
            backend="codex",
            model="gpt-test",
            codex_command=(sys.executable, "-c", backend),
        )

    receipt_paths = list((cache / "_usage" / "receipts").glob("*.json"))
    assert len(receipt_paths) == 1
    receipt_path = receipt_paths[0]
    assert f"failed usage receipt: {receipt_path}" in str(caught.value)
    assert receipt_path.is_file()
    receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
    assert receipt["status"] == "failed"
    assert receipt["usage"]["input_tokens"] == 53
    assert receipt["usage"]["output_tokens"] == 9
    assert receipt["usage"]["total_tokens"] == 62

    audit_path = (
        cache / "_usage" / "backend_outputs" / f"{receipt['receipt_id']}.json"
    )
    audit_text = audit_path.read_text(encoding="utf-8")
    audit = json.loads(audit_text)
    raw_output = "not valid structured JSON"
    assert set(audit) == {
        "schema_version",
        "receipt_id",
        "status",
        "raw_output_sha256",
        "raw_output",
        "jobs",
    }
    assert audit["status"] == "failed"
    assert audit["receipt_id"] == receipt["receipt_id"]
    assert audit["raw_output"] == raw_output
    assert audit["raw_output_sha256"] == hashlib.sha256(
        raw_output.encode("utf-8")
    ).hexdigest()
    assert audit["jobs"] == [
        {
            "key": job.key,
            "start_ms": job.start_ms,
            "end_ms": job.end_ms,
            "rejected_bullets": [],
        }
    ]
    assert "BEGIN_JOBS_JSON" not in audit_text
    assert "prompt" not in audit
    assert "stdout" not in audit


def test_repaired_backend_output_is_audited_and_cache_hit_is_stable(
    tmp_path: Path,
) -> None:
    cache = tmp_path / "cache"
    call_log = tmp_path / "calls.log"
    job = replace(
        _job("agent-a"),
        prior_context="PROMPT-ONLY-SECRET must not enter the backend-output audit.",
    )
    backend = f"""
import json
import sys
from pathlib import Path

prompt = sys.stdin.read()
payload_text = prompt.split("BEGIN_JOBS_JSON\\n", 1)[1].split("END_JOBS_JSON", 1)[0]
job = json.loads(payload_text)[0]
payload = {{"summaries": [{{
    "key": job["key"],
    "phrase": "MODEL-ONLY rejected prose",
    "paragraph": "MODEL-ONLY rejected prose must be replaced.",
    "work_summary": [
        {{"at_ms": job["start_ms"], "text": "Kept in-range work."}},
        {{"at_ms": job["end_ms"], "text": "Rejected end-boundary work."}},
    ],
}}]}}
raw_output = json.dumps(payload, sort_keys=True)
args = sys.argv[1:]
Path(args[args.index("--output-last-message") + 1]).write_text(
    raw_output, encoding="utf-8"
)
with Path({str(call_log)!r}).open("a", encoding="utf-8") as handle:
    handle.write("CALL\\n")
print(json.dumps({{
    "type": "turn.completed",
    "usage": {{
        "input_tokens": 41,
        "cached_input_tokens": 0,
        "output_tokens": 13,
    }},
}}))
"""

    first, first_stats = summarize_jobs(
        [job],
        cache,
        backend="codex",
        model="gpt-test",
        max_workers=1,
        codex_command=(sys.executable, "-c", backend),
    )

    result = first[job.key]
    assert result.input_hash == _input_hash(job, "codex", "gpt-test")
    assert result.prompt_version == PROMPT_VERSION
    assert result.phrase == "Kept in-range work."
    assert result.paragraph == "Kept in-range work."
    assert [(bullet.at_ms, bullet.text) for bullet in result.work_summary] == [
        (job.start_ms, "Kept in-range work.")
    ]
    assert result.artifact_provenance is not None
    receipt_id = result.artifact_provenance.usage_receipt_id
    assert receipt_id is not None
    audit_path = cache / "_usage" / "backend_outputs" / f"{receipt_id}.json"
    audit_bytes = audit_path.read_bytes()
    audit = json.loads(audit_bytes)
    raw_output = audit["raw_output"]
    assert isinstance(raw_output, str)
    assert set(audit) == {
        "schema_version",
        "receipt_id",
        "status",
        "raw_output_sha256",
        "raw_output",
        "jobs",
    }
    assert audit["schema_version"] == 1
    assert audit["receipt_id"] == receipt_id
    assert audit["status"] == "repaired"
    assert audit["raw_output_sha256"] == hashlib.sha256(
        raw_output.encode("utf-8")
    ).hexdigest()
    assert audit["jobs"] == [
        {
            "key": job.key,
            "start_ms": job.start_ms,
            "end_ms": job.end_ms,
            "rejected_bullets": [
                {
                    "index": 1,
                    "at_ms": job.end_ms,
                    "action": "dropped-out-of-range",
                }
            ],
        }
    ]
    audit_text = audit_bytes.decode("utf-8")
    assert "PROMPT-ONLY-SECRET" not in audit_text
    assert "BEGIN_JOBS_JSON" not in audit_text
    assert "prompt" not in audit
    assert "stdout" not in audit
    assert first_stats.hits == 0
    assert first_stats.misses == 1
    assert call_log.read_text(encoding="utf-8") == "CALL\n"

    second, second_stats = summarize_jobs(
        [job],
        cache,
        backend="codex",
        model="gpt-test",
        max_workers=1,
        codex_command=(sys.executable, "-c", backend),
    )

    assert second == first
    assert second_stats.hits == 1
    assert second_stats.misses == 0
    assert second_stats.batches == 0
    assert call_log.read_text(encoding="utf-8") == "CALL\n"
    assert audit_path.read_bytes() == audit_bytes


def test_failed_invocation_keeps_independently_validated_batch(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    first = _job("agent-a")
    failing = _job("agent-b")
    backend = """
import json
import sys
from pathlib import Path

prompt = sys.stdin.read()
if '"key":"agent-b"' in prompt or '"key": "agent-b"' in prompt:
    sys.stderr.write("deliberate second-batch failure")
    raise SystemExit(7)
payload_text = prompt.split("BEGIN_JOBS_JSON\\n", 1)[1].split("END_JOBS_JSON", 1)[0]
job = json.loads(payload_text)[0]
result = {"summaries": [{
    "key": job["key"],
    "phrase": "Preserved successful batch",
    "paragraph": "Validated work remains reusable after another batch fails.",
    "work_summary": [],
}]}
args = sys.argv[1:]
Path(args[args.index("--output-last-message") + 1]).write_text(json.dumps(result))
"""
    with pytest.raises(SummaryError, match="failed with exit 7") as caught:
        summarize_jobs(
            [first, failing],
            cache,
            backend="codex",
            model="gpt-test",
            max_workers=1,
            batch_size=1,
            codex_command=(sys.executable, "-c", backend),
        )
    assert "deliberate second-batch failure" not in str(caught.value)

    first_cache = cache / f"{_input_hash(first, 'codex', 'gpt-test')}.json"
    failing_cache = cache / f"{_input_hash(failing, 'codex', 'gpt-test')}.json"
    assert first_cache.is_file()
    assert not failing_cache.exists()


def _pending(job: SummaryJob, tmp_path: Path) -> _PendingJob:
    input_hash = _input_hash(job, "codex", "gpt-test")
    return _PendingJob(job, input_hash, tmp_path / f"{input_hash}.json")


@pytest.mark.parametrize(
    "payload",
    [
        {"summaries": [], "extra": True},
        {
            "summaries": [
                {"key": "agent-a", "phrase": "short", "work_summary": []}
            ]
        },
        {
            "summaries": [
                {
                    "key": "unexpected",
                    "phrase": "short",
                    "paragraph": "paragraph",
                    "work_summary": [],
                }
            ]
        },
        {
            "summaries": [
                {
                    "key": "agent-a",
                    "phrase": "x" * 81,
                    "paragraph": "paragraph",
                    "work_summary": [],
                }
            ]
        },
        {
            "summaries": [
                {
                    "key": "agent-a",
                    "phrase": "short",
                    "paragraph": "paragraph",
                    "work_summary": [{"at_ms": "not-an-int", "text": "work"}],
                }
            ]
        },
    ],
)
def test_backend_output_is_strictly_narrowed(
    payload: object, tmp_path: Path
) -> None:
    job = _job("agent-a")
    with pytest.raises(SummaryError):
        _parse_backend_output(
            json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
        )


def test_backend_bullets_are_stably_canonicalized_by_timestamp(tmp_path: Path) -> None:
    job = _job("agent-a")
    later_ms = job.start_ms + 30_000
    same_ms = job.start_ms + 10_000
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Ordered archival work",
                "paragraph": "The valid work items are normalized without another model call.",
                "work_summary": [
                    {"at_ms": later_ms, "text": "Finished validation."},
                    {"at_ms": same_ms, "text": "Started the implementation."},
                    {"at_ms": same_ms, "text": "Added focused tests."},
                ],
            }
        ]
    }

    result = _parse_backend_output(
        json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
    )[job.key]

    assert [(item.at_ms, item.text) for item in result.work_summary] == [
        (same_ms, "Started the implementation."),
        (same_ms, "Added focused tests."),
        (later_ms, "Finished validation."),
    ]


def test_backend_drops_out_of_range_bullet_and_rederives_model_prose(
    tmp_path: Path,
) -> None:
    job = _job("agent-a")
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Rejected material must not survive",
                "paragraph": "The rejected-only claim must not enter the archive.",
                "work_summary": [
                    {
                        "at_ms": job.end_ms,
                        "text": "Rejected-only claim from outside the interval.",
                    },
                    {
                        "at_ms": job.start_ms + 20_000,
                        "text": "Implemented deterministic timestamp recovery.",
                    },
                    {
                        "at_ms": job.start_ms,
                        "text": "Added focused interval tests.",
                    },
                ],
            }
        ]
    }

    result = _parse_backend_output(
        json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
    )[job.key]

    assert result.phrase == "Added focused interval tests."
    assert result.paragraph == (
        "Added focused interval tests. Implemented deterministic timestamp recovery."
    )
    assert [(bullet.at_ms, bullet.text) for bullet in result.work_summary] == [
        (job.start_ms, "Added focused interval tests."),
        (
            job.start_ms + 20_000,
            "Implemented deterministic timestamp recovery.",
        ),
    ]
    assert "Rejected" not in result.phrase + result.paragraph


def test_backend_all_rejected_bullets_use_transcript_only_fallback(
    tmp_path: Path,
) -> None:
    job = replace(
        _job(
            "agent-a",
            transcript="Implemented interval recovery. All 9 focused tests passed.",
        ),
        prior_context="PRIOR-CONTEXT-SECRET must never become current work.",
    )
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "REJECTED-PROSE",
                "paragraph": "REJECTED-PROSE must not survive fallback.",
                "work_summary": [
                    {
                        "at_ms": job.end_ms,
                        "text": "REJECTED-BULLET at the exclusive boundary.",
                    }
                ],
            }
        ]
    }

    result = _parse_backend_output(
        json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
    )[job.key]
    rendered = " ".join(
        [result.phrase, result.paragraph]
        + [bullet.text for bullet in result.work_summary]
    )

    assert "Implemented interval recovery" in rendered
    assert "9 focused tests passed" in rendered
    assert "PRIOR-CONTEXT-SECRET" not in rendered
    assert "REJECTED" not in rendered


def test_backend_interval_is_inclusive_at_start_exclusive_at_end(
    tmp_path: Path,
) -> None:
    job = _job("agent-a")
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Model prose is replaced after recovery",
                "paragraph": "Only the accepted start-boundary bullet may survive.",
                "work_summary": [
                    {"at_ms": job.start_ms, "text": "Accepted at interval start."},
                    {"at_ms": job.end_ms, "text": "Rejected at interval end."},
                ],
            }
        ]
    }

    result = _parse_backend_output(
        json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
    )[job.key]

    assert result.phrase == "Accepted at interval start."
    assert result.paragraph == "Accepted at interval start."
    assert [(bullet.at_ms, bullet.text) for bullet in result.work_summary] == [
        (job.start_ms, "Accepted at interval start.")
    ]


def test_backend_legitimate_empty_work_summary_retains_model_prose(
    tmp_path: Path,
) -> None:
    job = _job("agent-a")
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "No substantive phase work",
                "paragraph": "The model found no durable work in this interval.",
                "work_summary": [],
            }
        ]
    }

    result = _parse_backend_output(
        json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
    )[job.key]

    assert result.phrase == "No substantive phase work"
    assert result.paragraph == "The model found no durable work in this interval."
    assert result.work_summary == ()


def test_timestamp_recovery_does_not_mask_other_schema_errors(
    tmp_path: Path,
) -> None:
    job = _job("agent-a")
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Invalid mixed output",
                "paragraph": "A malformed bullet keeps this response invalid.",
                "work_summary": [
                    {"at_ms": job.end_ms, "text": "Recoverable timestamp."},
                    {"at_ms": job.start_ms, "text": 42},
                ],
            }
        ]
    }

    with pytest.raises(SummaryError, match="expected a string"):
        _parse_backend_output(
            json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
        )


def test_out_of_range_rollup_bullet_remains_a_hard_failure(tmp_path: Path) -> None:
    job = replace(_job("daily"), summary_style=TECHNICAL_ROLLUP_STYLE)
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Technical daily rollup",
                "paragraph": "The rollup still obeys strict timestamp validation.",
                "work_summary": [
                    {"at_ms": job.end_ms, "text": "Outside the interval."},
                ],
            }
        ]
    }

    with pytest.raises(SummaryError, match="outside the summary interval"):
        _parse_backend_output(
            json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
        )


def test_valid_backend_output_is_narrowed_to_frozen_types(tmp_path: Path) -> None:
    job = _job("agent-a")
    payload = {
        "summaries": [
            {
                "key": job.key,
                "phrase": "Built timeline cache",
                "paragraph": "Implemented and verified the timeline summary cache.",
                "work_summary": [
                    {"at_ms": job.start_ms, "text": "Implemented the cache."},
                    {"at_ms": job.end_ms - 1, "text": "Verified cache hits."},
                ],
            }
        ]
    }
    result = _parse_backend_output(
        json.dumps(payload), [_pending(job, tmp_path)], "gpt-test", "generated"
    )[job.key]
    assert result.model == "gpt-test"
    assert result.prompt_version == PROMPT_VERSION
    assert isinstance(result.work_summary, tuple)
    assert result.work_summary[-1].at_ms == job.end_ms - 1
