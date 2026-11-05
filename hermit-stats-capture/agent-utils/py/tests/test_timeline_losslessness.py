"""The losslessness gate: payload storage, and the Codex row-by-row differential over it."""

from __future__ import annotations

import hashlib
import json
import shutil
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path

import pytest

import wrkviz.losslessness as losslessness_module
from wrkviz.build_store import team_build_root
from wrkviz.cli import main as timeline_main
from wrkviz.losslessness import (
    audit_archive_losslessness,
    audit_codex_losslessness,
    format_losslessness_audit,
    unscope_codex_id,
)
from wrkviz.model import PayloadRef
from wrkviz.payloads import (
    load_payload_manifest,
    merge_payloads,
    payload_digest,
    read_payload,
    read_payload_digests,
    resolve_payloads,
    shard_name,
    verify_payload_store,
)
from wrkviz.pipeline import (
    ingest_codex,
    load_archived_team,
    rehydrate_tool_payloads,
)
from tests.timeline_snapshots import snapshot_root

ROOT = "root-thread"
CHILD = "child-thread"

#: The exec argument and the exec output used throughout. They are deliberately not tiny: the
#: point of the payload store is bulk, and a one-character fixture would pass a byte-length check
#: by accident.
COMMAND = json.dumps({"cmd": ["bash", "-lc", "make validate"], "workdir": "/work/project"})
STDOUT = "ok\n" * 400 + "warning: /home/private/customer/project is not a repository\n"


def _iso(seconds: float) -> str:
    return datetime.fromtimestamp(seconds, timezone.utc).isoformat().replace("+00:00", "Z")


def _line(timestamp: float, kind: str, payload: dict[str, object]) -> bytes:
    record: dict[str, object] = {
        "timestamp": _iso(timestamp),
        "type": kind,
        "payload": payload,
    }
    return json.dumps(record, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"


def _root_bytes(*, extra: bytes = b"") -> bytes:
    return (
        b"".join(
            (
                _line(
                    1_000,
                    "session_meta",
                    {
                        "id": ROOT,
                        "session_id": ROOT,
                        "timestamp": _iso(1_000),
                        "cwd": "/home/private/customer/project",
                        "git": {"repository_url": "git@github.com:rrnewton/dev-widget.git"},
                        "source": "cli",
                    },
                ),
                _line(
                    1_001,
                    "event_msg",
                    {"type": "task_started", "turn_id": "root-turn", "started_at": 1_001},
                ),
                _line(
                    1_002,
                    "response_item",
                    {
                        "type": "message",
                        "id": "root-answer",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "Root done"}],
                    },
                ),
                _line(
                    1_002.1,
                    "response_item",
                    {
                        "type": "custom_tool_call",
                        "id": "tool-item",
                        "call_id": "call-exec",
                        "name": "exec",
                        "input": COMMAND,
                    },
                ),
                _line(
                    1_002.2,
                    "response_item",
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call-exec",
                        "output": STDOUT,
                    },
                ),
                _line(
                    1_003,
                    "event_msg",
                    {
                        "type": "task_complete",
                        "turn_id": "root-turn",
                        "started_at": 1_001,
                        "completed_at": 1_003,
                    },
                ),
            )
        )
        + extra
    )


def _child_bytes() -> bytes:
    """A subagent session whose rollout opens with the parent's replayed turn."""

    return b"".join(
        (
            _line(
                1_010,
                "session_meta",
                {
                    "id": CHILD,
                    "session_id": ROOT,
                    "parent_thread_id": ROOT,
                    "timestamp": _iso(1_010),
                    "agent_path": "/root/worker",
                    "source": {
                        "subagent": {
                            "thread_spawn": {
                                "parent_thread_id": ROOT,
                                "agent_path": "/root/worker",
                                "depth": 1,
                            }
                        }
                    },
                },
            ),
            # Imported: the parent's turn, replayed into the child's file before its own work.
            _line(
                1_001,
                "event_msg",
                {"type": "task_started", "turn_id": "root-turn", "started_at": 1_001},
            ),
            _line(
                1_011,
                "event_msg",
                {"type": "task_started", "turn_id": "child-turn", "started_at": 1_011},
            ),
            _line(
                1_012,
                "response_item",
                {
                    "type": "message",
                    "id": "child-answer",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Child done"}],
                },
            ),
        )
    )


def _ingest(tmp_path: Path, *, extra: bytes = b"", child: bool = False) -> Path:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    day = sessions / "2026" / "08" / "05"
    day.mkdir(parents=True, exist_ok=True)
    (day / "rollout-root.jsonl").write_bytes(_root_bytes(extra=extra))
    if child:
        (day / "rollout-child.jsonl").write_bytes(_child_bytes())
    ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")
    return archive


def _payload_root(archive: Path) -> Path:
    return team_build_root(archive, "codex-test") / "payloads"


# --------------------------------------------------------------------------------------------
# The payload store itself.
# --------------------------------------------------------------------------------------------


def test_ingest_keeps_tool_text_the_archived_model_no_longer_inlines(tmp_path: Path) -> None:
    """The defect this whole change exists for: the text used to be deleted outright."""

    archive = _ingest(tmp_path)
    team = load_archived_team(archive, "codex-test")
    (tool,) = team.tool_calls

    # Still not inline -- that part of the old behaviour is deliberate and unchanged.
    assert tool.input_text is None
    assert tool.output_text is None
    # But no longer gone: the model names the text and says how much of it there is.
    assert tool.input_payload == PayloadRef(payload_digest(COMMAND), len(COMMAND.encode()))
    assert tool.output_payload == PayloadRef(
        payload_digest(STDOUT), len(STDOUT.encode("utf-8"))
    )
    assert read_payload(_payload_root(archive), tool.input_payload.sha256) == COMMAND
    assert read_payload(_payload_root(archive), tool.output_payload.sha256) == STDOUT
    assert rehydrate_tool_payloads(archive, team).tool_calls[0].output_text == STDOUT


def test_payload_tree_is_gitignored_and_tracked_files_stay_clean(tmp_path: Path) -> None:
    """The separation is the point: bulk out of version control, model in it, cwd nowhere."""

    archive = _ingest(tmp_path)
    assert (archive / ".gitignore").read_text(encoding="utf-8").splitlines() == [
        "/.wrkviz.lock",
        "/teams/*/source_snapshots/",
        "/teams/*/payloads/",
        # Local recovery state, like the lock: `gc --delete` renames what it reclaims into here
        # rather than unlinking it, and a quarter-gigabyte of reclaimed files must not turn up
        # as a proposed addition to the operator's next commit.
        "/.wrkviz-trash/",
    ]
    tracked = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (team_build_root(archive, "codex-test") / "raw").rglob("*")
        if path.is_file()
    )
    # The stdout the payload tree now holds is exactly the kind of content that must not reach a
    # tracked file, so the fixture puts a home-directory path inside it and this asserts on that.
    assert "/home/private/customer/project" not in tracked
    assert "warning: /home/private" not in tracked
    assert payload_digest(STDOUT) in tracked  # the digest is fine; the text is not
    stored = read_payload(_payload_root(archive), payload_digest(STDOUT))
    assert stored is not None
    assert "/home/private/customer/project" in stored


def test_repeat_ingest_stores_nothing_new_and_rewrites_no_shard(tmp_path: Path) -> None:
    archive = _ingest(tmp_path)
    before = {
        path: path.read_bytes() for path in sorted(_payload_root(archive).rglob("*")) if path.is_file()
    }

    _, repeat = ingest_codex(
        archive, tmp_path / "sessions", ROOT, "codex-test", "UTC"
    )

    assert repeat.newly_stored_tool_payloads == 0
    assert repeat.tool_payloads == 2
    assert repeat.files_changed == 0
    after = {
        path: path.read_bytes() for path in sorted(_payload_root(archive).rglob("*")) if path.is_file()
    }
    assert after == before


def test_merge_never_shrinks_when_a_payload_stops_being_observed(tmp_path: Path) -> None:
    """A union, for the same reason task-note promotion is one: this tree is the only copy."""

    root = tmp_path / "payloads"
    merge_payloads(root, ("first", "second"))

    report = merge_payloads(root, ("second",))

    assert report.stored == 2
    assert report.newly_stored == 0
    assert read_payload(root, payload_digest("first")) == "first"


def test_merge_touches_only_the_shards_it_adds_to(tmp_path: Path) -> None:
    """Why the tree is sharded at all: one new payload must not rewrite the whole corpus."""

    root = tmp_path / "payloads"
    existing = [f"payload-{index}" for index in range(64)]
    merge_payloads(root, existing)
    stamps = {path: path.read_bytes() for path in sorted(root.glob("*.jsonl"))}
    assert len(stamps) > 1

    report = merge_payloads(root, [*existing, "brand new"])

    assert report.newly_stored == 1
    # At most the one shard the new digest lands in; the other 63 must be byte-identical, which
    # is what "one new command does not rewrite 290 MB" means concretely.
    added = root / shard_name(payload_digest("brand new"))
    changed = [path for path, before in stamps.items() if path.read_bytes() != before]
    assert changed in ([], [added])
    assert added.is_file()
    assert report.files_changed == 2  # the one shard, plus the manifest


def test_manifest_commits_to_the_tree_and_verification_catches_an_edit(tmp_path: Path) -> None:
    root = tmp_path / "payloads"
    merge_payloads(root, ("alpha", "beta"))
    manifest = load_payload_manifest(root)
    assert manifest is not None
    assert manifest.records == 2
    assert manifest.text_bytes == len("alpha") + len("beta")
    assert verify_payload_store(root) == ()

    shard = root / shard_name(payload_digest("alpha"))
    shard.write_text(
        shard.read_text(encoding="utf-8").replace('"text":"alpha"', '"text":"tampered"'),
        encoding="utf-8",
    )

    problems = verify_payload_store(root)
    assert any("hashes to" in problem for problem in problems)
    assert any("do not match the digest recorded for them" in problem for problem in problems)


def test_two_different_payloads_under_one_digest_are_refused(tmp_path: Path) -> None:
    root = tmp_path / "payloads"
    merge_payloads(root, ("alpha",))
    digest = payload_digest("alpha")
    shard = root / shard_name(digest)
    shard.write_text(
        shard.read_text(encoding="utf-8")
        + json.dumps({"sha256": digest, "text": "not alpha"}, separators=(",", ":"))
        + "\n",
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="two different payloads are stored under digest"):
        read_payload_digests(root)


def test_a_line_outside_the_canonical_shape_is_refused_rather_than_parsed(tmp_path: Path) -> None:
    """The fixed digest offset is a property of the format; a reader that guessed would drift."""

    root = tmp_path / "payloads"
    merge_payloads(root, ("alpha",))
    shard = root / shard_name(payload_digest("alpha"))
    shard.write_text(
        json.dumps({"text": "alpha", "sha256": payload_digest("alpha")}, indent=1) + "\n",
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="does not have the canonical shape"):
        read_payload_digests(root)


def test_an_empty_payload_is_stored_rather_than_conflated_with_an_absent_one(
    tmp_path: Path,
) -> None:
    """`""` and `None` are different observations, and the old code could not tell them apart."""

    archive = _ingest(
        tmp_path,
        extra=_line(
            1_004,
            "response_item",
            {
                "type": "custom_tool_call",
                "id": "quiet-item",
                "call_id": "call-quiet",
                "name": "exec",
                "input": "",
            },
        ),
    )
    team = load_archived_team(archive, "codex-test")
    quiet = next(tool for tool in team.tool_calls if tool.call_id == "call-quiet")

    assert quiet.input_payload == PayloadRef(payload_digest(""), 0)
    assert quiet.output_payload is None
    assert read_payload(_payload_root(archive), payload_digest("")) == ""


def test_resolve_payloads_reads_each_shard_once(tmp_path: Path) -> None:
    root = tmp_path / "payloads"
    texts = [f"payload-{index}" for index in range(32)]
    merge_payloads(root, texts)
    refs = [PayloadRef(payload_digest(text), len(text.encode())) for text in texts]

    resolved = resolve_payloads(root, [*refs, *refs])

    assert resolved == {payload_digest(text): text for text in texts}


def test_a_pruned_payload_tree_still_says_what_it_no_longer_holds(tmp_path: Path) -> None:
    """The reference outlives the bytes on purpose; that is the whole reason it carries a length."""

    archive = _ingest(tmp_path)
    shutil.rmtree(_payload_root(archive))

    team = load_archived_team(archive, "codex-test")
    (tool,) = team.tool_calls
    assert tool.output_payload == PayloadRef(
        payload_digest(STDOUT), len(STDOUT.encode("utf-8"))
    )
    rehydrated = rehydrate_tool_payloads(archive, team).tool_calls[0]
    assert rehydrated.output_text is None
    assert rehydrated.output_payload == tool.output_payload


# --------------------------------------------------------------------------------------------
# The differential.
# --------------------------------------------------------------------------------------------


def test_every_vendor_row_is_accounted_for_and_every_claim_holds(tmp_path: Path) -> None:
    archive = _ingest(tmp_path, child=True)

    report = audit_codex_losslessness(archive, "codex-test")

    assert report.covered
    assert report.unaccounted == ()
    assert report.unverified == ()
    assert report.sound
    assert report.vendor_files == 2
    assert report.vendor_rows == 10
    assert {tally.name for tally in report.tallies} >= {
        "session-metadata",
        "turn-start",
        "turn-complete",
        "assistant-message",
        "tool-call",
        "tool-output",
        "imported-turn-prefix",
    }
    # The payload rules are the ones this change added, and they are checked byte for byte.
    payload_rules = [tally for tally in report.tallies if tally.disposition == "payload"]
    assert sum(tally.rows for tally in payload_rules) == 2
    assert all(tally.unverified == 0 for tally in payload_rules)


def test_an_unknown_vendor_row_shape_fails_the_gate_loudly(tmp_path: Path) -> None:
    """The whole purpose: a record type nobody taught the reader about must not pass silently."""

    archive = _ingest(
        tmp_path,
        extra=_line(
            1_004,
            "event_msg",
            {"type": "brand_new_thing_the_reader_ignores", "body": "irreplaceable"},
        ),
    )

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.sound
    assert report.unaccounted_rows == 1
    (finding,) = report.unaccounted
    assert finding.payload_type == "brand_new_thing_the_reader_ignores"
    assert "no rule in the Codex table accounts for this row shape" in finding.detail


def test_a_lost_payload_fails_the_gate_rather_than_reading_as_a_quiet_tool_call(
    tmp_path: Path,
) -> None:
    archive = _ingest(tmp_path)
    (_payload_root(archive) / shard_name(payload_digest(STDOUT))).unlink()

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.sound
    assert report.unverified_rows == 1
    assert any("absent from the store" in finding.detail for finding in report.unverified)
    assert report.payload_problems


def test_a_payload_that_is_not_the_text_it_claims_fails_the_gate(tmp_path: Path) -> None:
    """A digest match is not enough; the differential re-derives the text from the vendor row."""

    archive = _ingest(tmp_path)
    shard = _payload_root(archive) / shard_name(payload_digest(STDOUT))
    shard.write_text(
        shard.read_text(encoding="utf-8").replace(
            json.dumps(STDOUT), json.dumps(STDOUT.replace("ok", "no"))
        ),
        encoding="utf-8",
    )

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.sound
    assert any(
        "does not match the text this row carries" in finding.detail
        for finding in report.unverified
    )


def test_reverting_to_the_old_behaviour_fails_the_gate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The regression the gate exists to catch, staged exactly: null the text again.

    This is what ``_archive_team`` did before this change, on every tool call in every archive. It
    left no error, no warning and no diff anyone would read -- which is why an assertion in the
    ingest path would not have helped and a differential over the vendor rows does.
    """

    archive = _ingest(tmp_path)
    team_path = team_build_root(archive, "codex-test") / "raw" / "team.json"
    document = json.loads(team_path.read_text(encoding="utf-8"))
    for tool in document["tool_calls"]:
        tool.pop("input_payload", None)
        tool.pop("output_payload", None)
    team_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.sound
    assert report.unverified_rows == 2
    assert all(
        "has no stored payload for text this row carries" in finding.detail
        for finding in report.unverified
    )


def test_a_streamed_interim_tool_output_is_declared_rather_than_left_unverified(
    tmp_path: Path,
) -> None:
    """Found by running the gate on a real 1.1 GiB team; kept so it stays declared, not silent.

    A long-running command that streams progress writes one output row per update and the reader
    keeps only the last. The rows this loses are genuinely lost, so they belong in the inventory --
    but they must not read as UNVERIFIED, because a gate that is permanently red for a cause
    somebody already understood is a gate nobody looks at.
    """

    archive = _ingest(
        tmp_path,
        extra=b"".join(
            (
                _line(
                    1_002.3,
                    "response_item",
                    {
                        "type": "custom_tool_call",
                        "id": "streaming-item",
                        "call_id": "call-stream",
                        "name": "exec",
                        "input": COMMAND,
                    },
                ),
                _line(
                    1_002.4,
                    "response_item",
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call-stream",
                        "output": "10/20 verified",
                    },
                ),
                _line(
                    1_002.5,
                    "response_item",
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call-stream",
                        "output": "20/20 verified",
                    },
                ),
            )
        ),
    )

    report = audit_codex_losslessness(archive, "codex-test")

    assert report.sound
    assert report.unverified == ()
    (tally,) = [item for item in report.tallies if item.name == "superseded-tool-output"]
    assert tally.rows == 1
    assert tally.disposition == "dropped"
    # Only the surviving output is stored, and the gate says which one that is rather than
    # implying the interim text is somewhere.
    stream = next(
        tool
        for tool in load_archived_team(archive, "codex-test").tool_calls
        if tool.call_id == "call-stream"
    )
    assert stream.output_payload is not None
    assert read_payload(_payload_root(archive), stream.output_payload.sha256) == "20/20 verified"
    assert read_payload(_payload_root(archive), payload_digest("10/20 verified")) is None


def test_the_declared_inventory_is_what_still_blocks_deleting_the_snapshots(
    tmp_path: Path,
) -> None:
    """A sound archive is not automatically a complete one, and the report must not blur them."""

    archive = _ingest(
        tmp_path,
        extra=_line(
            1_004,
            "event_msg",
            {
                "type": "token_count",
                "info": {"total_token_usage": {"total_tokens": 14_090}},
            },
        ),
    )

    report = audit_codex_losslessness(archive, "codex-test")

    assert report.sound
    assert not report.lossless
    assert report.absent_rows == 1
    (tally,) = [item for item in report.tallies if item.name == "token-count"]
    assert tally.disposition == "dropped"
    assert tally.blocks_deletion
    assert tally.vendor_bytes == report.absent_bytes


def test_the_cwd_redaction_is_declared_rather_than_counted_as_a_loss(tmp_path: Path) -> None:
    """A policy redaction and a silent drop look identical in a diff; only one is acceptable."""

    archive = _ingest(tmp_path)

    report = audit_codex_losslessness(archive, "codex-test")

    (tally,) = [item for item in report.tallies if item.name == "session-metadata"]
    assert tally.disposition == "redacted"
    assert not tally.blocks_deletion
    assert "test_ingest_never_persists_cwd_or_repository_credentials" in tally.reason
    assert report.lossless  # nothing in this fixture is dropped, and the redaction is not a drop


def test_an_uncovered_provider_is_named_rather_than_omitted(tmp_path: Path) -> None:
    archive = _ingest(tmp_path)
    manifest_path = team_build_root(archive, "codex-test") / "raw" / "source-manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["provider"] = "orc"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    report = audit_archive_losslessness(archive)

    (team,) = report.reports
    assert not team.covered
    assert team.skipped is not None
    assert "only codex is covered" in team.skipped
    # An uncovered team cannot fail the gate, and it cannot let the archive claim losslessness.
    assert report.sound
    assert not report.lossless
    assert "not covered" in format_losslessness_audit(report, "text")


def test_absent_snapshots_are_reported_as_nothing_left_to_compare(tmp_path: Path) -> None:
    """After the deletion this gate exists to authorize, the gate has no opinion left to give."""

    archive = _ingest(tmp_path)
    shutil.rmtree(snapshot_root(archive, "codex-test"))

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.covered
    assert report.skipped is not None
    assert "already absent" in report.skipped


def test_unscoping_a_continuation_id_is_exact_and_leaves_others_alone() -> None:
    lineage = "successor-root"
    raw = "root-turn"
    scoped = f"codex-continuation-{len(lineage)}-{lineage}-{len(raw)}-{raw}"

    assert unscope_codex_id(scoped) == raw
    assert unscope_codex_id(raw) == raw
    # A string that only looks like the prefix must survive untouched, or the audit would compare
    # a mangled id against the model and report a drop that is not there.
    assert unscope_codex_id("codex-continuation-not-a-length") == (
        "codex-continuation-not-a-length"
    )
    assert unscope_codex_id("codex-continuation-2-ab-99-short") == (
        "codex-continuation-2-ab-99-short"
    )


def test_every_rule_that_claims_retention_names_a_check() -> None:
    """The table's own invariant: a rule may not claim more than its verification proves."""

    # The verifications that compare content rather than merely finding a citation. A rule whose
    # reason promises the bytes survived has to name one of these, because "some record in the
    # model mentions this line" is true of a record whose text has been blanked -- which is the
    # regression `test_blanking_every_message_fails_the_gate` stages, and which the table's own
    # invariant is supposed to make unreachable.
    content_checks = {
        "prompt-text",
        "content-text",
        "objective-text",
        "input-payload",
        "output-payload",
    }
    for rule in losslessness_module._CODEX_RULES:
        if rule.disposition in {"record", "payload", "duplicate"}:
            assert rule.verification != "none", rule.name
        if rule.disposition in {"record", "payload"} and "verbatim" in rule.reason:
            assert rule.verification in content_checks, rule.name
        if rule.verification in {"model-id", "corpus-id"}:
            assert rule.id_path, rule.name
        assert rule.reason.strip(), rule.name
    # And every shape has exactly one unselected rule at most, so the table cannot silently
    # shadow a later entry with an earlier catch-all.
    for shape, rules in losslessness_module._RULES_BY_SHAPE.items():
        defaults = [rule for rule in rules if rule.selector is None]
        assert len(defaults) <= 1, shape
        assert not defaults or rules[-1].selector is None, shape


def test_the_cli_exits_one_on_a_finding_and_zero_on_a_sound_archive(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    archive = _ingest(tmp_path)

    assert timeline_main(["audit-losslessness", "--output", str(archive)]) == 0
    sound = capsys.readouterr()
    assert "sound: yes" in sound.out

    # `--require-lossless` is the flag an actual deletion would be gated on, and this archive is
    # sound without being complete.
    assert (
        timeline_main(
            [
                "audit-losslessness",
                "--output",
                str(archive),
                "--require-lossless",
                "--format",
                "json",
            ]
        )
        == 0
    )
    document = json.loads(capsys.readouterr().out)
    assert document["schema_version"] == 1
    assert document["lossless"] is True

    (_payload_root(archive) / shard_name(payload_digest(STDOUT))).unlink()
    assert timeline_main(["audit-losslessness", "--output", str(archive)]) == 1
    assert "AUDIT FAILED" in capsys.readouterr().out


def test_a_missing_archive_exits_two_rather_than_reading_as_a_finding(tmp_path: Path) -> None:
    """Two exit codes, because "the gate says no" and "the gate did not run" are different."""

    assert timeline_main(["audit-losslessness", "--output", str(tmp_path / "nope")]) == 2


def test_the_generation_marker_commits_to_the_payload_tree(tmp_path: Path) -> None:
    """Not a Codex concern -- the marker is Orc's -- but the digest it now carries is measurable."""

    root = tmp_path / "payloads"
    merge_payloads(root, ("alpha",))
    first = load_payload_manifest(root)
    assert first is not None
    merge_payloads(root, ("alpha", "beta"))
    second = load_payload_manifest(root)
    assert second is not None

    assert first.digest() != second.digest()
    assert second.digest() == hashlib.sha256(
        (json.dumps(second.to_json_obj(), ensure_ascii=False, indent=2, sort_keys=True) + "\n")
        .encode("utf-8")
    ).hexdigest()


def test_a_tool_call_with_no_text_carries_no_reference(tmp_path: Path) -> None:
    archive = _ingest(tmp_path)
    team = load_archived_team(archive, "codex-test")
    (tool,) = team.tool_calls

    stripped = replace(tool, input_payload=None, output_payload=None)

    assert "input_payload" not in stripped.to_json_obj()
    assert "output_payload" not in stripped.to_json_obj()
    assert "input_payload" in tool.to_json_obj()


# --------------------------------------------------------------------------------------------
# What the store and the gate do when something outside them has already gone wrong.
#
# Each of these stages a state the design says is either supported (a prune) or impossible (a
# shard that is not its digest), and asserts the behaviour the module documents rather than the
# behaviour it happened to have. They exist because the first draft got three of them backwards:
# it refused the supported operation permanently, absorbed the impossible one in silence, and
# defined its own corpus as whatever survived on disk.
# --------------------------------------------------------------------------------------------


#: The three characters `str.splitlines` treats as line terminators and JSON does not escape.
#: `canonical_jsonl` writes with `ensure_ascii=False`, so all three reach the disk raw.
HOSTILE = "before\u2028after\u2029next\u0085last"


def test_a_payload_holding_a_unicode_line_separator_survives_the_round_trip(
    tmp_path: Path,
) -> None:
    """U+2028 is a line terminator to `str.splitlines` and an ordinary character to JSON.

    Tool output is arbitrary text from arbitrary programs. A shard written with one of these in it
    and read back with `splitlines` becomes two fragments, neither of which has the canonical
    shape, and from then on every read, ingest and audit of the team raises with no way back.
    """

    root = tmp_path / "payloads"
    report = merge_payloads(root, (HOSTILE, "ordinary"))

    assert report.stored == 2
    assert read_payload(root, payload_digest(HOSTILE)) == HOSTILE
    assert verify_payload_store(root) == ()
    # And a second merge reads what the first wrote, which is where the damage actually surfaced.
    assert merge_payloads(root, ("ordinary",)).newly_stored == 0
    assert read_payload_digests(root) == frozenset(
        {payload_digest(HOSTILE), payload_digest("ordinary")}
    )


def test_a_pruned_shard_is_reported_and_does_not_wedge_the_next_ingest(
    tmp_path: Path,
) -> None:
    """Pruning the tree is documented as supported; it must not be a one-way door."""

    archive = _ingest(tmp_path)
    root = _payload_root(archive)
    pruned = shard_name(payload_digest(STDOUT))
    (root / pruned).unlink()

    _, report = ingest_codex(archive, tmp_path / "sessions", ROOT, "codex-test", "UTC")

    assert report.pruned_payload_shards == (pruned,)
    # The very ingest that met the prune re-observed the same bytes and put them back, which is
    # what makes refusing here indefensible as well as unrecoverable.
    assert read_payload(root, payload_digest(STDOUT)) == STDOUT
    assert verify_payload_store(root) == ()
    assert load_archived_team(archive, "codex-test").tool_calls


def test_a_shard_that_lost_records_is_reported_rather_than_laundered(
    tmp_path: Path,
) -> None:
    """The next merge must not rewrite the manifest over a shard that shrank beneath it."""

    root = tmp_path / "payloads"
    # Two payloads whose digests share a first byte, so the loss is a shrunken shard rather than
    # a missing one: the same file has to come back with fewer records in it.
    both = ("payload-13", "payload-33")
    assert shard_name(payload_digest(both[0])) == shard_name(payload_digest(both[1]))
    merge_payloads(root, both)
    shard = root / shard_name(payload_digest(both[0]))
    kept = [
        line
        for line in shard.read_text(encoding="utf-8").split("\n")
        if line and payload_digest(both[1]) not in line
    ]
    surviving = len(kept)
    shard.write_text("".join(f"{line}\n" for line in kept), encoding="utf-8")
    assert verify_payload_store(root) != ()

    report = merge_payloads(root, (both[0],))

    assert report.damaged_shards and shard.name in report.damaged_shards[0]
    # The manifest is now honest about what is left instead of carrying the old byte count over a
    # file that no longer holds it.
    manifest = load_payload_manifest(root)
    assert manifest is not None
    assert manifest.records == surviving
    assert verify_payload_store(root) == ()


def test_a_deleted_vendor_snapshot_fails_the_gate_rather_than_shrinking_the_corpus(
    tmp_path: Path,
) -> None:
    """Deletion must not be self-ratifying: the manifest, not `rglob`, defines the corpus."""

    archive = _ingest(tmp_path, child=True)
    snapshots = snapshot_root(archive, "codex-test")
    intact = audit_codex_losslessness(archive, "codex-test")
    assert intact.vendor_files == 2 and intact.source_problems == ()

    (snapshots / "2026" / "08" / "05" / "rollout-child.jsonl").unlink()
    partial = audit_codex_losslessness(archive, "codex-test")

    assert partial.vendor_files == 1
    assert not partial.sound
    assert partial.source_problems == (
        "2026/08/05/rollout-child.jsonl: recorded in raw/source-manifest.json but absent "
        "from source_snapshots/",
    )

    # ... and emptying the tree entirely reads as two losses, not as a clean bill of health.
    (snapshots / "2026" / "08" / "05" / "rollout-root.jsonl").unlink()
    emptied = audit_codex_losslessness(archive, "codex-test")
    assert emptied.vendor_rows == 0
    assert not emptied.sound
    assert len(emptied.source_problems) == 2
    assert timeline_main(
        ["audit-losslessness", "--require-lossless", "--output", str(archive)]
    ) == 1


def test_a_truncated_vendor_snapshot_is_caught_by_its_recorded_digest(
    tmp_path: Path,
) -> None:
    """`_complete_prefix` drops an incomplete tail silently, so enumeration proves nothing."""

    archive = _ingest(tmp_path)
    rollout = (
        snapshot_root(archive, "codex-test")
        / "2026"
        / "08"
        / "05"
        / "rollout-root.jsonl"
    )
    blob = rollout.read_bytes()
    rollout.write_bytes(blob[: blob.rindex(b"\n", 0, len(blob) - 1) + 1])

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.sound
    assert len(report.source_problems) == 1
    assert "bytes on disk, the manifest records" in report.source_problems[0]


def test_blanking_every_message_fails_the_gate(tmp_path: Path) -> None:
    """The same regression as `test_reverting_to_the_old_behaviour_fails_the_gate`, on messages.

    Six of the seven `record` rules say the row's text is carried verbatim, and for a long time
    all six were checked by a rule that only asked whether *something* in the model cited the
    line. That check passes on an archive whose every message body has been replaced with the
    empty string -- which is the exact shape of the drop this module exists to find.
    """

    archive = _ingest(tmp_path, child=True)
    assert audit_codex_losslessness(archive, "codex-test").sound

    team_path = team_build_root(archive, "codex-test") / "raw" / "team.json"
    document = json.loads(team_path.read_text(encoding="utf-8"))
    for event in document["events"]:
        event["text"] = ""
        event["encrypted_content"] = None
    team_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")

    report = audit_codex_losslessness(archive, "codex-test")

    assert not report.sound
    assert report.unverified_rows
    assert {finding.payload_type for finding in report.unverified} <= {
        "user_message",
        "message",
        "agent_message",
    }
    assert any("the row carries" in finding.detail for finding in report.unverified)
    assert timeline_main(
        ["audit-losslessness", "--require-lossless", "--output", str(archive)]
    ) == 1


def test_a_payload_manifest_written_before_the_rename_is_still_read(tmp_path: Path) -> None:
    """`payloads/manifest.json` carries `"tool"`, and every one on disk predates the rename.

    This was a total ingestion outage, not a cosmetic slip: all twelve teams failed `normalize`
    with "invalid payload manifest" against files whose bytes were entirely valid.

    It was missed because the rename's sweep for stored identifiers grepped the PUBLISHED
    archive -- and the payload store had just been relocated out of it into `<output>.build`. The
    lesson generalises past this one file: a rename must sweep the build store and the snapshot
    store as well as the directory that ships.
    """

    from wrkviz.payloads import load_payload_manifest

    root = tmp_path / "payloads"
    root.mkdir()
    (root / "manifest.json").write_text(
        '{"schema_version": 1, "tool": "agent-team-timeline", "records": 0,'
        ' "text_bytes": 0, "shards": []}\n',
        encoding="utf-8",
    )
    manifest = load_payload_manifest(root)
    assert manifest is not None, "a pre-rename payload manifest must still load"

    # Current spelling too, and a foreign one still refused -- leniency about the rename is not
    # leniency about whose manifest this is.
    (root / "manifest.json").write_text(
        '{"schema_version": 1, "tool": "wrkviz", "records": 0, "text_bytes": 0, "shards": []}\n',
        encoding="utf-8",
    )
    assert load_payload_manifest(root) is not None
    (root / "manifest.json").write_text(
        '{"schema_version": 1, "tool": "something-else", "records": 0,'
        ' "text_bytes": 0, "shards": []}\n',
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="invalid payload manifest"):
        load_payload_manifest(root)
