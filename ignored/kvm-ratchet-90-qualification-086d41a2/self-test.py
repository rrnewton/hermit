#!/usr/bin/env python3
"""Pure fixture tests for the KVM ratchet campaign gates.

This creates only temporary JSON/TSV/filesystem evidence. It never builds or
invokes Hermit, KVM, systemd, the hermetic runner, or the attributed launcher.
"""

from __future__ import annotations

import hashlib
import html
import importlib.util
import base64
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
import fcntl
import zlib
from contextlib import redirect_stdout
from collections import Counter
from copy import deepcopy
from pathlib import Path
from unittest import mock

import yaml

sys.dont_write_bytecode = True


SCRIPT_DIR = Path(__file__).resolve().parent
ROOT = SCRIPT_DIR.parents[1]
EXPECTED = SCRIPT_DIR / "expected-cells.json"
RESULT_VALIDATOR = SCRIPT_DIR / "validate-results.jq"
EVIDENCE_VALIDATOR = SCRIPT_DIR / "validate-evidence.sh"
STRICT_ARTIFACT_VALIDATOR = SCRIPT_DIR / "validate-strict-invocation-artifacts.sh"
LAUNCHER = SCRIPT_DIR / "launch.py"
DRIVER = SCRIPT_DIR / "run-kvm-ratchet-90.sh"
FREEZE_MANIFEST = SCRIPT_DIR / "FROZEN_SHA256SUMS"
SOURCE_SHA = "086d41a2f2f76e5a2cccceea342feb6957311c2b"
SOURCE_TREE = "94490fd2cee758395150590a8ba25ff86443dbfd"
MACHINE = "devbig014"
PREFIX = "kvm-ratchet-90-086d41a2-run6"
IMAGE = (
    "localhost/hermit-hermetic-validate@sha256:"
    "e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985"
)
BINARY_BYTES = b"synthetic retained hermit binary\n"
BINARY_SHA = hashlib.sha256(BINARY_BYTES).hexdigest()
ARTIFACT_PLACEHOLDER = "@ARTIFACT@"
SHELL_SAFE_WORD = re.compile(r"^[A-Za-z0-9_@%+=:,./-]+$")


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest_file(path: Path) -> str:
    return digest_bytes(path.read_bytes())


def serialize_disk_report(document: object) -> str:
    return json.dumps(document, separators=(",", ":"), sort_keys=True) + "\n"


def canonical_runtime() -> dict[str, object]:
    return {
        "run1": {
            "scheduler_turns": 11,
            "virtual_nanoseconds": 22,
            "syscalls": 33,
        },
        "run2": {
            "scheduler_turns": 11,
            "virtual_nanoseconds": 22,
            "syscalls": 33,
        },
    }


def instantiate_guest_argv(
    template: list[str], artifact_dir: str
) -> list[str]:
    return [
        artifact_dir + argument[len(ARTIFACT_PLACEHOLDER) :]
        if argument.startswith(ARTIFACT_PLACEHOLDER)
        else argument
        for argument in template
    ]


def source_guest_contracts() -> dict[str, dict[str, object]]:
    """Reproduce runner.rs guest construction and test_digest from source YAML."""
    contracts: dict[str, dict[str, object]] = {}
    manifest_dir = ROOT / "tests/e2e/manifests"
    for manifest_path in sorted(manifest_dir.glob("*.yaml")):
        if manifest_path.name == "defaults.yaml":
            continue
        document = yaml.safe_load(manifest_path.read_text())
        for recipe in document["test"]:
            test_id = recipe["id"]
            if test_id in contracts:
                raise AssertionError(f"duplicate source recipe: {test_id}")
            mode = recipe["modes"]["verify"]
            guest_args = list(mode.get("guest_args", {}).get("kvm", []))
            program = recipe.get("program")
            direct = recipe.get("direct")
            if program is not None:
                program_bytes = (ROOT / program).read_bytes()
                test_sha256 = digest_bytes(program_bytes)
                if program.endswith((".c", ".rs")):
                    guest = [f"{ARTIFACT_PLACEHOLDER}/fixtures/program"]
                elif program.endswith(".sh"):
                    guest = [f"/src/{program}", "--run"]
                else:
                    raise AssertionError(
                        f"unsupported source program kind for {test_id}: {program}"
                    )
            elif isinstance(direct, str):
                direct_vector = [direct]
                test_sha256 = digest_bytes(
                    json.dumps(
                        direct_vector, separators=(",", ":"), ensure_ascii=False
                    ).encode()
                )
                guest = ["bash", "-c", direct]
                if guest_args:
                    guest.append("--")
            elif isinstance(direct, list) and all(
                isinstance(argument, str) for argument in direct
            ):
                direct_vector = list(direct)
                test_sha256 = digest_bytes(
                    json.dumps(
                        direct_vector, separators=(",", ":"), ensure_ascii=False
                    ).encode()
                )
                guest = direct_vector
            else:
                raise AssertionError(f"unsupported source recipe for {test_id}")
            guest.extend(guest_args)
            resolved_guest: list[str] = []
            for argument in guest:
                if (
                    argument.startswith("/")
                    or argument in (".", "..")
                    or argument.startswith(ARTIFACT_PLACEHOLDER)
                ):
                    resolved_guest.append(argument)
                    continue
                resolved = ROOT / argument
                looks_like_repo_path = (
                    argument.startswith("./")
                    or "/" in argument
                    or resolved.is_file()
                )
                if looks_like_repo_path and resolved.exists():
                    # PathBuf::join retains an explicit leading `./` component.
                    resolved_guest.append(f"/src/{argument}")
                else:
                    resolved_guest.append(argument)
            contracts[test_id] = {
                "guest_argv_template": resolved_guest,
                "test_sha256": test_sha256,
            }
    return contracts


def producer_shell_quote(value: str) -> str:
    if value and SHELL_SAFE_WORD.fullmatch(value):
        return value
    return "'" + value.replace("'", "'\"'\"'") + "'"


def producer_shell_command(
    cwd: str, environment: dict[str, str], argv: list[str]
) -> str:
    words = ["cd", producer_shell_quote(cwd), "&&", "env"]
    words.extend(
        producer_shell_quote(f"{name}={environment[name]}")
        for name in sorted(environment)
    )
    words.extend(producer_shell_quote(argument) for argument in argv)
    return " ".join(words)


def canonical_comparison(info_messages: object) -> dict[str, object]:
    return {
        "strictness": "canonical",
        "display_name": "BitwiseInfoV1",
        "compare_logs": True,
        "compare_io_buffers": True,
        "log_scope": "info",
        "record_envelope": "all_records_v1",
        "virtualize_time": True,
        "strip_lines": False,
        "canonicalize_addresses": True,
        "full_trace": True,
        "exact_remainder": True,
        "stripped_prefixes": ["real-wall-clock-prefix/v1"],
        "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
        "ignore_lines": False,
        "skip_commit": False,
        "skip_detlog": False,
    }


def report(kind: str, info_messages: object = 3) -> dict[str, object]:
    common: dict[str, object] = {
        "infrastructure_error": None,
        "first_divergent_scheduler_turn": None,
        "first_divergent_virtual_nanoseconds": None,
        "first_divergent_record": None,
        "first_divergent_syscall": None,
        "first_divergent_left_message": None,
        "first_divergent_right_message": None,
    }
    if kind in (
        "pass",
        "cpu_overrun",
        "timeout_signaled_match",
        "timeout_zero_match",
    ):
        matched_count = 0 if kind == "timeout_zero_match" else info_messages
        return common | {
            "verified": True,
            "bitwise_parity": matched_count != 0,
            "verdict": "matched",
            "comparison": canonical_comparison(info_messages),
            "compared_log_messages": {
                "left": matched_count,
                "right": matched_count,
            },
            "guest_exit_code": 0,
            "guest_signal": None,
            "runtime": canonical_runtime(),
        }
    if kind in (
        "divergence",
        "timeout_divergence",
        "timeout_left_zero_divergence",
        "timeout_zero_divergence",
    ):
        left_count = (
            0
            if kind in ("timeout_left_zero_divergence", "timeout_zero_divergence")
            else info_messages
        )
        right_count = 0 if kind == "timeout_zero_divergence" else info_messages
        return common | {
            "verified": False,
            "bitwise_parity": False,
            "verdict": "diverged",
            "comparison": canonical_comparison(info_messages),
            "compared_log_messages": {
                "left": left_count,
                "right": right_count,
            },
            "guest_exit_code": 0,
            "guest_signal": None,
            "runtime": canonical_runtime(),
            "first_divergent_record": (
                None
                if kind == "timeout_zero_divergence"
                else 1 if kind == "timeout_left_zero_divergence" else 7
            ),
            "first_divergent_left_message": (
                "INFO left record"
                if kind in ("divergence", "timeout_divergence")
                else None
            ),
            "first_divergent_right_message": (
                "INFO right-only record"
                if kind == "timeout_left_zero_divergence"
                else "INFO right record"
                if kind in ("divergence", "timeout_divergence")
                else None
            ),
        }
    if kind in ("timeout", "pre_timeout"):
        return common | {
            "verified": False,
            "bitwise_parity": False,
            "verdict": "no_result",
            "no_result_reason": {"kind": "not_run"},
            "comparison": None,
            "compared_log_messages": None,
            "guest_exit_code": None,
            "guest_signal": None,
        }
    if kind in ("crash", "timeout_first_run"):
        return common | {
            "verified": False,
            "bitwise_parity": False,
            "verdict": "no_result",
            "no_result_reason": {
                "kind": "first_run_rejected",
                "exit_code": 1,
                "signal": None,
                "stdout_bytes": 0,
                "stderr_bytes": 17,
            },
            "comparison": None,
            "compared_log_messages": None,
            "guest_exit_code": 1,
            "guest_signal": None,
        }
    raise AssertionError(kind)


def make_row(
    cell: dict[str, str],
    run_index: int,
    attempt: int,
    kind: str,
    *,
    kernel: str,
    info_messages: object = 3,
) -> dict[str, object]:
    test = cell["test"]
    slug = test.replace("/", "-")
    run_id = f"{PREFIX}-{slug}-repetition-{run_index}"
    artifact_suffix = "" if attempt == 1 else f"-attempt-{attempt}"
    artifact_dir = f"/results/runs/{run_id}/{slug}-verify-kvm{artifact_suffix}"
    guest_argv = instantiate_guest_argv(
        cell["guest_argv_template"], artifact_dir
    )
    execution_env = {
        "E2E_FIXTURE_DIR": f"{artifact_dir}/fixtures",
        "E2E_TMPDIR": "/tmp/hermit-e2e",
        "HERMIT_E2E_SCHEDULED_JOBS": "1",
        "HOME": f"{artifact_dir}/home",
        "LC_ALL": "C",
        "TZ": "UTC",
        "XDG_CONFIG_HOME": f"{artifact_dir}/xdg-config",
    }
    argv = [
        "/src/target/release/hermit",
        "--log",
        "info",
        "run",
        "--base-env=minimal",
        "--backend",
        "kvm",
        "--strict",
        "--verify-strict",
        "--verify",
        "--verify-json",
        f"{artifact_dir}/verify-1.json",
        "--keep-logs",
        "--verify-log-dir",
        f"{artifact_dir}/verify-logs/verify-1",
        "--mount=type=tmpfs,target=/test",
        "--workdir",
        "/test",
        "--env",
        "LC_ALL=C",
        "--env",
        "TZ=UTC",
        "--env",
        f"HOME={artifact_dir}/home",
        "--env",
        f"XDG_CONFIG_HOME={artifact_dir}/xdg-config",
        "--env",
        "E2E_TMPDIR=/test",
        "--env",
        f"E2E_FIXTURE_DIR={artifact_dir}/fixtures",
        "--env",
        "HERMIT_E2E_SCHEDULED_JOBS=1",
        "--",
        *guest_argv,
    ]
    if kind == "timeout_no_report":
        report_document = None
        report_text = None
    else:
        report_document = report(kind, info_messages)
        report_text = serialize_disk_report(report_document)
        if kind != "pre_timeout":
            pass
        else:
            report_text = report_text.removesuffix("\n")
    runtime = None if report_document is None else report_document.get("runtime")
    if kind == "pass":
        outcome, result, failure_class, error_kind = "PASS", "pass", None, None
        status, signal, timed_out, reason = 0, None, False, None
    elif kind == "divergence":
        outcome, result = "FAIL", "determinism-failure"
        failure_class, error_kind = "product_failure", None
        status, signal, timed_out = 1, None, False
        reason = "canonical verification diverged"
    elif kind == "timeout":
        outcome, result = "ERROR", "timeout"
        failure_class, error_kind = "no_result", "wall-timeout"
        status, signal, timed_out = None, 15, True
        reason = "cell exceeded 57 wall s backstop (22 s CPU budget)"
    elif kind == "cpu_overrun":
        outcome, result = "FAIL", "timeout"
        failure_class, error_kind = "no_result", "cpu-timeout"
        status, signal, timed_out = 0, None, True
        reason = "cell exceeded 22 CPU s"
    elif kind == "pre_timeout":
        outcome, result = "ERROR", "timeout"
        failure_class, error_kind = "no_result", "wall-timeout"
        status, signal, timed_out = None, None, True
        reason = "cell exceeded 57 wall s backstop (22 s CPU budget) before attempt 1 started"
    elif kind == "timeout_first_run":
        outcome, result = "ERROR", "timeout"
        failure_class, error_kind = "no_result", "cpu-timeout"
        status, signal, timed_out = 125, None, True
        reason = "cell exceeded 22 CPU s"
    elif kind == "timeout_divergence":
        outcome, result = "FAIL", "timeout"
        failure_class, error_kind = "no_result", "cpu-timeout"
        status, signal, timed_out = 1, None, True
        reason = "cell exceeded 22 CPU s"
    elif kind in ("timeout_left_zero_divergence", "timeout_zero_divergence"):
        outcome, result = "ERROR", "timeout"
        failure_class, error_kind = "no_result", "cpu-timeout"
        status, signal, timed_out = 1, None, True
        reason = "cell exceeded 22 CPU s"
    elif kind == "timeout_signaled_match":
        outcome, result = "FAIL", "timeout"
        failure_class, error_kind = "no_result", "wall-timeout"
        status, signal, timed_out = None, 15, True
        reason = "cell exceeded 57 wall s backstop (22 s CPU budget)"
    elif kind == "timeout_zero_match":
        outcome, result = "ERROR", "timeout"
        failure_class, error_kind = "no_result", "cpu-timeout"
        status, signal, timed_out = 0, None, True
        reason = "cell exceeded 22 CPU s"
    elif kind == "timeout_no_report":
        outcome, result = "ERROR", "timeout"
        failure_class, error_kind = "no_result", "wall-timeout"
        status, signal, timed_out = None, 9, True
        reason = "cell exceeded 57 wall s backstop (22 s CPU budget)"
    elif kind == "crash":
        outcome, result = "FAIL", "crash-error"
        failure_class, error_kind = "product_failure", None
        status, signal, timed_out = 125, None, False
        reason = "verify exited before producing a terminal comparison"
    else:
        raise AssertionError(kind)
    attempt_record = {
        "index": "1",
        "outcome": outcome,
        "status": status,
        "signal": signal,
        "timed_out": timed_out,
        "error_kind": error_kind,
        "reason": reason,
        "duration_ms": 1,
        "cpu_usage_usec": (
            None
            if kind == "pre_timeout"
            else 22_000_000
            if kind
            in (
                "cpu_overrun",
                "timeout_first_run",
                "timeout_divergence",
                "timeout_left_zero_divergence",
                "timeout_zero_divergence",
                "timeout_zero_match",
            )
            else 1
        ),
        "observation_sha256": digest_bytes(
            f"{test}|{run_index}|{attempt}|{kind}|observation".encode()
        ),
        "argv": argv,
        "guest_argv": guest_argv,
        "env": execution_env,
        "cwd": "/src",
        "shell_command": producer_shell_command("/src", execution_env, argv),
        "stdout": "",
        "stderr": "",
        "verification_report": report_text,
        "verification_report_sha256": (
            None if report_text is None else digest_bytes(report_text.encode())
        ),
        "runtime": deepcopy(runtime),
        "first_divergent_scheduler_turn": None,
        "first_divergent_virtual_nanoseconds": None,
        "first_divergent_record": (
            None
            if kind == "timeout_zero_divergence"
            else 1
            if kind == "timeout_left_zero_divergence"
            else 7
            if kind in ("divergence", "timeout_divergence")
            else None
        ),
        "first_divergent_syscall": None,
        "first_divergent_left_message": (
            "INFO left record"
            if kind in ("divergence", "timeout_divergence")
            else None
        ),
        "first_divergent_right_message": (
            "INFO right-only record"
            if kind == "timeout_left_zero_divergence"
            else "INFO right record"
            if kind in ("divergence", "timeout_divergence")
            else None
        ),
        "sabre_path_evidence": None,
        "sabre_path_evidence_sha256": None,
    }
    return {
        "schema": 4,
        "hermit_sha": SOURCE_SHA,
        "source_tree_dirty": False,
        "binary_build_sha": SOURCE_SHA[:12],
        "binary_sha256": BINARY_SHA,
        "test_sha256": cell["test_sha256"],
        "machine_shortname": MACHINE,
        "kernel_version": kernel,
        "host_capabilities": {"kvm": {"present": True}},
        "lane": "portable",
        "category": cell["category"],
        "test": test,
        "mode": "verify",
        "backend": "kvm",
        "classification": (
            "disabled" if cell["selector"] == "--probe-disabled" else "required"
        ),
        "attempt": attempt,
        "run_index": run_index,
        "run_id": run_id,
        "artifact_dir": artifact_dir,
        "log_level": "info",
        "relaxations": [],
        "argv": argv,
        "effective_args": argv[1:],
        "guest_argv": attempt_record["guest_argv"],
        "env": attempt_record["env"],
        "cwd": attempt_record["cwd"],
        "shell_command": attempt_record["shell_command"],
        "execution_cpu_timeout_seconds": 22,
        "execution_wall_timeout_seconds": 57,
        "timeout_seconds": 57,
        "duration_ms": 57_000 if error_kind == "wall-timeout" else 1,
        "cpu_usage_usec": (
            None
            if kind == "pre_timeout"
            else 22_000_000
            if kind
            in (
                "cpu_overrun",
                "timeout_first_run",
                "timeout_divergence",
                "timeout_left_zero_divergence",
                "timeout_zero_divergence",
                "timeout_zero_match",
            )
            else 1
        ),
        "runtime": deepcopy(runtime),
        "execution_path": None,
        "diversity": None,
        "outcome": outcome,
        "result": result,
        "failure_class": failure_class,
        "error_kind": error_kind,
        "reason": reason,
        "attempts": [attempt_record],
        "first_divergent_scheduler_turn": None,
        "first_divergent_virtual_nanoseconds": None,
        "first_divergent_record": (
            None
            if kind == "timeout_zero_divergence"
            else 1
            if kind == "timeout_left_zero_divergence"
            else 7
            if kind in ("divergence", "timeout_divergence")
            else None
        ),
        "first_divergent_syscall": None,
        "first_divergent_left_message": (
            "INFO left record"
            if kind in ("divergence", "timeout_divergence")
            else None
        ),
        "first_divergent_right_message": (
            "INFO right-only record"
            if kind == "timeout_left_zero_divergence"
            else "INFO right record"
            if kind in ("divergence", "timeout_divergence")
            else None
        ),
    }


def jq_validate(rows: list[dict[str, object]], kernel: str) -> dict[str, object]:
    proc = subprocess.run(
        [
            "jq",
            "-s",
            "--arg",
            "operation",
            "validate",
            "--slurpfile",
            "expected",
            str(EXPECTED),
            "--arg",
            "source_sha",
            SOURCE_SHA,
            "--arg",
            "machine",
            MACHINE,
            "--arg",
            "kernel",
            kernel,
            "--arg",
            "campaign_prefix",
            PREFIX,
            "--arg",
            "test",
            "",
            "--argjson",
            "run_index",
            "0",
            "-f",
            str(RESULT_VALIDATOR),
        ],
        input="".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows),
        text=True,
        capture_output=True,
        check=True,
    )
    return json.loads(proc.stdout)


def jq_eligible(rows: list[dict[str, object]], cell: dict[str, str], kernel: str) -> bool:
    proc = subprocess.run(
        [
            "jq",
            "-s",
            "-e",
            "--arg",
            "operation",
            "eligibility",
            "--slurpfile",
            "expected",
            str(EXPECTED),
            "--arg",
            "source_sha",
            SOURCE_SHA,
            "--arg",
            "machine",
            MACHINE,
            "--arg",
            "kernel",
            kernel,
            "--arg",
            "campaign_prefix",
            PREFIX,
            "--arg",
            "test",
            cell["test"],
            "--argjson",
            "run_index",
            "1",
            "-f",
            str(RESULT_VALIDATOR),
        ],
        input="".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows),
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode not in (0, 1):
        raise RuntimeError(proc.stderr)
    return proc.returncode == 0


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)
    print(f"PASS {message}")


def sync_invocation_argv(row: dict[str, object]) -> None:
    """Keep the row/attempt copies aligned after an intentional argv mutation."""
    row["effective_args"] = row["argv"][1:]
    row["attempts"][0]["argv"] = deepcopy(row["argv"])


def validate_evidence(evidence: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(EVIDENCE_VALIDATOR)],
        env=os.environ | {"KVM_QUALIFICATION_CAMPAIGN": str(evidence)},
        text=True,
        capture_output=True,
        check=False,
    )


def load_launcher(*, trusted: bool = False):
    spec = importlib.util.spec_from_file_location("kvm_ratchet_launcher", LAUNCHER)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load launcher for the run-record discriminator")
    module = importlib.util.module_from_spec(spec)
    if trusted:
        launcher_source = LAUNCHER.read_bytes()
        freeze_source = FREEZE_MANIFEST.read_bytes()
        module._KVM_RATCHET_CAMPAIGN_DIR = str(SCRIPT_DIR)
        module._KVM_RATCHET_CAPSULE_SHA256 = digest_bytes(launcher_source)
        module._KVM_RATCHET_CAPSULE_SOURCE = launcher_source
        module._KVM_RATCHET_FREEZE_MANIFEST_SHA256 = digest_bytes(freeze_source)
        module._KVM_RATCHET_FREEZE_MANIFEST_SOURCE = freeze_source
    spec.loader.exec_module(module)
    return module


def invocation_gate(
    row_strict: str,
    artifacts_strict: str,
    actual_rc: int,
    expected_rc: int,
    infrastructure_fault_delta: int = 0,
    leaked_summaries: int = 0,
) -> bool:
    completed = subprocess.run(
        [
            "bash",
            str(DRIVER),
            "--self-test-invocation-gate",
            row_strict,
            artifacts_strict,
            str(actual_rc),
            str(expected_rc),
            str(infrastructure_fault_delta),
            str(leaked_summaries),
        ],
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode not in (0, 1):
        raise RuntimeError(completed.stderr)
    return completed.returncode == 0


def write_jsonl(path: Path, rows: list[dict[str, object]]) -> None:
    path.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows))


def excluded_round1_rows(
    cells: list[dict[str, str]], kernel: str
) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for cell in cells:
        rows.append(make_row(cell, 1, 1, "timeout", kernel=kernel))
        rows.append(make_row(cell, 1, 2, "timeout", kernel=kernel))
    return rows


def without_invocation(
    rows: list[dict[str, object]], test: str, run_index: int
) -> list[dict[str, object]]:
    return [
        row
        for row in rows
        if not (row["test"] == test and row["run_index"] == run_index)
    ]


def replace_round1_invocation(
    rows: list[dict[str, object]],
    cell: dict[str, str],
    kind: str,
    kernel: str,
    *,
    info_messages: object = 3,
) -> list[dict[str, object]]:
    replaced = without_invocation(rows, cell["test"], 1)
    replaced.extend(
        make_row(
            cell,
            1,
            attempt,
            kind,
            kernel=kernel,
            info_messages=info_messages,
        )
        for attempt in (1, 2)
    )
    return replaced


def static_population_document() -> dict[str, object]:
    proc = subprocess.run(
        ["bash", str(SCRIPT_DIR / "run-kvm-ratchet-90.sh"), "--static-check"],
        text=True,
        capture_output=True,
        check=True,
    )
    return json.loads(proc.stdout)


def rebuild_artifact_inventory(evidence: Path) -> None:
    results_root = evidence / "results"
    inventory = ["sha256\tpath\n"]
    artifact_files = [path for path in results_root.rglob("*") if path.is_file()]
    for path in sorted(artifact_files, key=lambda value: os.fsencode(str(value))):
        inventory.append(f"{digest_file(path)}\t{path.relative_to(evidence)}\n")
    (evidence / "artifact-hashes.tsv").write_text("".join(inventory))
    (evidence / "artifact-hashes.stderr").write_text("")


def materialize_evidence_fixture(
    root: Path, cells: list[dict[str, str]], kernel: str, *, advancer_count: int = 2
) -> Path:
    evidence = root / "evidence"
    results_root = evidence / "results"
    results_root.mkdir(parents=True)
    retained_binary = evidence / "split/target/release/hermit"
    retained_binary.parent.mkdir(parents=True)
    retained_binary.write_bytes(BINARY_BYTES)
    retained_binary.chmod(0o755)
    (evidence / "built-binary.tsv").write_text(
        "sha256\tpath\n"
        f"{BINARY_SHA}\tsplit/target/release/hermit\n"
    )
    static_document = static_population_document()
    (evidence / "static-population-check.json").write_text(
        json.dumps(static_document, indent=2, sort_keys=True) + "\n"
    )
    (evidence / "source-status.before").write_text("")
    (evidence / "source-status.before.rc").write_text("0\n")
    (evidence / "source-status.before.stderr").write_text("")
    (evidence / "source-status.after").write_text("")
    (evidence / "source-status.after.rc").write_text("0\n")
    (evidence / "source-status.after.stderr").write_text("")
    (evidence / "pinned-environment.log").write_text(
        "ldd (GNU libc) 2.42\n"
        "container_kvm_type=character special file\n"
        "container_kvm_mode=666\n"
        "container_rlimit_fsize_soft_bytes=1073742000\n"
        "container_rlimit_fsize_hard_bytes=1073742000\n"
    )
    (evidence / "environment.log").write_text(
        f"source_sha={SOURCE_SHA}\n"
        f"source_tree={SOURCE_TREE}\n"
        "source_status=clean\n"
        f"image_digest={IMAGE}\n"
        f"machine_shortname={MACHINE}\n"
        f"kernel_version={kernel}\n"
        "kvm_type=character special file\n"
        "kvm_mode=666\n"
        "pinned_environment_rc=0\n"
        "backend=kvm\nmode=verify\nlog_level=info\nrelaxations=none\n"
        "retained_log_max_bytes=1073741824\n"
        "invocation_log_max_bytes=1073741824\n"
        "capture_file_max_bytes=1073741824\n"
        "service_log_max_bytes=1073741824\n"
        "process_file_limit_bytes=1073742000\n"
        "service_log_path=/home/newton/work/dev-hermit/ignored/validate/hermit-kvm-ratchet-90-086d41a2-run6.log\n"
        "host_rlimit_fsize_soft_bytes=1073742000\n"
        "host_rlimit_fsize_hard_bytes=1073742000\n"
        "campaign_budget_bytes=137438953472\n"
        "filesystem_reserve_bytes=137438953472\n"
        "initial_campaign_allocated_bytes=4096\n"
        "initial_filesystem_free_bytes=1099511627776\n"
        "prior_campaign_allocated_bytes=4688396288\n"
        "prior_campaign_results_allocated_bytes=93057024\n"
        "retained_log_truncation_marker_bytes=176\n"
        "retained_log_single_max_with_marker_bytes=1073742000\n"
        "retained_log_theoretical_count=1190\n"
        "retained_log_theoretical_ceiling_bytes=1277752980000\n"
        "campaign_budget_policy=early-global-invalid-stop-not-full-theoretical-retention\n"
        "resource_guard_policy=nominal-one-second-polling-process-group-stop-preserve-prefix-invalidate-campaign\n"
        "resource_guard_scan_latency=synchronous-filesystem-scan-not-a-one-second-latency-guarantee\n"
        "cpu_timeout_multiplier=1\nwall_timeout_multiplier=1\n"
        "round1_cell_count=119\nround1_probe_disabled=102\n"
        "round1_include_manual=17\n"
        "continuation_rule=exactly-one-attempt-1-canonical-pass\n"
        "deadline_prior_outer_invocations=120\n"
        "deadline_prior_attempts=130\n"
        "deadline_prior_elapsed_seconds=1169\n"
        "deadline_max_outer_invocations=357\n"
        "deadline_max_attempts=595\n"
        "deadline_retry_aware_projection_seconds=5350\n"
        "deadline_hard_attempt_exposure_seconds=33915\n"
        "deadline_policy=operational-stop-for-investigation\n"
        "invocation_backstop_seconds=600\n"
        "invocation_retry_aware_modeled_seconds=248\n"
        "invocation_backstop_policy=infrastructure-only\n"
        "campaign_child_deadline_seconds=7200\n"
        f"payload_sha256={digest_file(DRIVER)}\n"
        f"freeze_manifest_sha256={digest_file(FREEZE_MANIFEST)}\n"
        "runtime_input_source=sealed-memfd\n"
        "reviewed_input_trust=external-launch-and-freeze-digests-plus-argv-capsule-and-post-boundary-sealed-runtime-memfds\n"
        "external_measurement_trust=with-proxy,systemd,python-stdlib,validate-lock,bash,jq,git,pinned-checkout-and-toolchain\n"
        "harness_jobs=1\n"
    )
    (evidence / "phase-status.tsv").write_text(
        "phase\trc\telapsed_seconds\nfetch\t0\t1\nbuild\t0\t1\n"
        "manifest_validate\t0\t1\npreparation\t0\t1\n"
    )
    frozen_hashes = static_document["hashes"]
    frozen_header = (
        "phase\texpected_cells_sha256\tpopulation_validator_sha256\t"
        "results_validator_sha256\tevidence_validator_sha256\t"
        "strict_artifact_validator_sha256\n"
    )
    frozen_lines = [frozen_header]
    for phase in ("round-1", "round-2", "round-3", "final"):
        frozen_lines.append(
            f"{phase}\t{frozen_hashes['expected_cells']}\t"
            f"{frozen_hashes['population_validator']}\t"
            f"{frozen_hashes['results_validator']}\t"
            f"{frozen_hashes['evidence_validator']}\t"
            f"{frozen_hashes['strict_artifact_validator']}\n"
        )
    (evidence / "frozen-input-checks.tsv").write_text("".join(frozen_lines))
    payload_hash = digest_file(DRIVER)
    (evidence / "payload-input-checks.tsv").write_text(
        "phase\texpected_sha256\tobserved_sha256\n"
        + "".join(
            f"{phase}\t{payload_hash}\t{payload_hash}\n"
            for phase in ("startup", "round-1", "round-2", "round-3", "final")
        )
    )
    (evidence / "round-cleanliness.tsv").write_text(
        "phase\tsource_sha\tsource_tree\tstatus\n"
        f"round-1\t{SOURCE_SHA}\t{SOURCE_TREE}\tclean\n"
        f"round-2\t{SOURCE_SHA}\t{SOURCE_TREE}\tclean\n"
        f"round-3\t{SOURCE_SHA}\t{SOURCE_TREE}\tclean\n"
    )
    (evidence / "population.tsv").write_text(
        "test\tselector\n"
        + "".join(f"{cell['test']}\t{cell['selector']}\n" for cell in cells)
    )
    (evidence / "round1-eligible.tsv").write_text(
        "test\tselector\n"
        + "".join(
            f"{cell['test']}\t{cell['selector']}\n"
            for cell in cells[:advancer_count]
        )
    )
    continuation_invocations = advancer_count * 2
    expected_invocations = 119 + continuation_invocations
    (evidence / "execution-complete.txt").write_text(
        f"round1_invocations=119\nround1_eligible={advancer_count}\n"
        f"continuation_invocations={continuation_invocations}\n"
        f"expected_invocations={expected_invocations}\n"
        f"recorded_invocations={expected_invocations}\n"
        "driver_infrastructure_faults=0\nresource_guard_hits=0\n"
        "evidence_complete=true\n"
    )
    (evidence / "resource-guard.tsv").write_text(
        "timestamp_epoch_seconds\tscope\treason\tobserved_bytes\tlimit_bytes\tpath\n"
    )
    (evidence / "disk-budget.tsv").write_text(
        "phase\tallocated_bytes\tlogical_bytes\tfilesystem_free_bytes\t"
        "service_log_bytes\tcampaign_budget_bytes\tfilesystem_reserve_bytes\t"
        "service_log_max_bytes\tguard_hits\tstatus\n"
        "initial\t4096\t4096\t1099511627776\t0\t137438953472\t"
        "137438953472\t1073741824\t0\tclear\n"
        "final\t1073741824\t5368709120\t1099511627776\t4096\t"
        "137438953472\t137438953472\t1073741824\t0\tclear\n"
    )
    (evidence / "artifact-inventory-wrapper.log").write_text("")
    (evidence / "strict-validation-wrapper.log").write_text("")

    aggregate: list[bytes] = []
    artifact_lines = [
        "test\trun_index\tattempt\tresults_jsonl\tresults_sha256\tartifact_dir\t"
        "artifact_state\tverification_report\tverification_report_sha256\t"
        "run1_log\trun2_log\treport_staging_file\n"
    ]
    invocation_lines = [
        "test\tselector\trepetition\trc\telapsed_seconds\tresult_rows\t"
        "strict_single_pass\tleaked_summary_count\tresult_dir\n"
    ]
    advancers = cells[:advancer_count]
    invocation_specs: list[tuple[dict[str, str], int, list[str]]] = []
    for index, cell in enumerate(cells):
        if index < advancer_count:
            invocation_specs.append((cell, 1, ["pass"]))
        elif index == advancer_count:
            invocation_specs.append((cell, 1, ["crash", "crash"]))
        elif index == advancer_count + 1:
            invocation_specs.append((cell, 1, ["cpu_overrun", "cpu_overrun"]))
        elif index == advancer_count + 2:
            invocation_specs.append((cell, 1, ["pre_timeout", "pre_timeout"]))
        elif index == advancer_count + 3:
            invocation_specs.append((cell, 1, ["timeout", "timeout"]))
        elif index == advancer_count + 4:
            invocation_specs.append(
                (cell, 1, ["timeout_first_run", "timeout_first_run"])
            )
        elif index == advancer_count + 5:
            invocation_specs.append(
                (cell, 1, ["timeout_divergence", "timeout_divergence"])
            )
        elif index == advancer_count + 6:
            invocation_specs.append(
                (cell, 1, ["timeout_signaled_match", "timeout_signaled_match"])
            )
        elif index == advancer_count + 7:
            invocation_specs.append(
                (
                    cell,
                    1,
                    ["timeout_left_zero_divergence", "timeout_left_zero_divergence"],
                )
            )
        elif index == advancer_count + 8:
            invocation_specs.append(
                (cell, 1, ["timeout_zero_divergence", "timeout_zero_divergence"])
            )
        elif index == advancer_count + 9:
            invocation_specs.append(
                (cell, 1, ["timeout_no_report", "timeout_no_report"])
            )
        elif index == advancer_count + 11:
            invocation_specs.append(
                (cell, 1, ["timeout_zero_match", "timeout_zero_match"])
            )
        elif index == advancer_count + 12:
            invocation_specs.append((cell, 1, ["divergence", "timeout"]))
        elif index == advancer_count + 13:
            invocation_specs.append((cell, 1, ["timeout", "divergence"]))
        elif index == advancer_count + 14:
            invocation_specs.append((cell, 1, ["divergence", "pass"]))
        else:
            invocation_specs.append((cell, 1, ["timeout", "timeout"]))
    for repetition in (2, 3):
        for cell in advancers:
            invocation_specs.append((cell, repetition, ["pass"]))

    for cell, repetition, kinds in invocation_specs:
        invocation_rows = []
        for attempt, kind in enumerate(kinds, start=1):
            row = make_row(cell, repetition, attempt, kind, kernel=kernel)
            if kind == "timeout_first_run" and attempt == 1:
                # A live timeout can interrupt after both temp logs exist;
                # normal status 125 follows cleanup and can retain only run1.
                row["attempts"][0]["status"] = None
                row["attempts"][0]["signal"] = 15
            invocation_rows.append(row)
        slug = cell["test"].replace("/", "-")
        result_dir = results_root / slug / f"repetition-{repetition}"
        result_dir.mkdir(parents=True)
        result_file = result_dir / "results.jsonl"
        write_jsonl(result_file, invocation_rows)
        result_bytes = result_file.read_bytes()
        aggregate.append(result_bytes)
        (result_dir / "invocation.log").write_text("synthetic typed campaign invocation\n")
        selected_outcome = next(
            outcome
            for outcome in ("PASS", "FAIL", "ERROR")
            if any(row["outcome"] == outcome for row in invocation_rows)
        )
        terminal_row = next(
            row
            for row in reversed(invocation_rows)
            if row["outcome"] == selected_outcome
        )
        terminal_outcome = str(terminal_row["outcome"])
        cpu_values = [row["cpu_usage_usec"] for row in invocation_rows]
        aggregate_cpu_usage = (
            None
            if any(value is None for value in cpu_values)
            else sum(int(value) for value in cpu_values)
        )
        (result_dir / "summary.json").write_text(
            json.dumps(
                {
                    "schema": 1,
                    "cells": 1,
                    "passed": 1 if terminal_outcome == "PASS" else 0,
                    "failed": 1 if terminal_outcome == "FAIL" else 0,
                    "errors": 1 if terminal_outcome == "ERROR" else 0,
                    "host_inapplicable": 0,
                    "cell_cpu_usage_usec": aggregate_cpu_usage,
                    "host_inapplicable_cells": [],
                },
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )
        failure_xml = ""
        if terminal_outcome == "FAIL":
            failure_xml = f"<failure>{html.escape(str(terminal_row['reason']))}</failure>"
        elif terminal_outcome == "ERROR":
            failure_xml = f"<error>{html.escape(str(terminal_row['reason']))}</error>"
        (result_dir / "junit.xml").write_text(
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            f'<testsuite name="hermit-e2e" tests="1" '
            f'failures="{1 if terminal_outcome == "FAIL" else 0}" '
            f'errors="{1 if terminal_outcome == "ERROR" else 0}" skipped="0">\n'
            f'  <testcase classname="{html.escape(cell["category"], quote=True)}" '
            f'name="{html.escape(cell["test"] + "/verify/kvm", quote=True)}" '
            f'time="{float(terminal_row["duration_ms"]) / 1000.0:.3f}">'
            f"{failure_xml}</testcase>\n</testsuite>\n"
        )
        leak_dir = result_dir / "leaked-private-summaries"
        leak_dir.mkdir()
        leak_lines = ["kind\tpath\tmtime_epoch_seconds\tsize_bytes\tsha256\n"]
        if cell["test"] == cells[8]["test"] and repetition == 1:
            for kind_name, basename, contents in (
                (
                    "verify",
                    ".hermit-verify-summary-fixture-one",
                    b"verify summary fixture\n",
                ),
                (
                    "verify",
                    ".hermit-verify-summary-fixture-two",
                    b"second verify summary fixture\n",
                ),
            ):
                leak_path = leak_dir / basename
                leak_path.write_bytes(contents)
                leak_lines.append(
                    f"{kind_name}\t{leak_path.relative_to(evidence)}\t"
                    f"{int(leak_path.stat().st_mtime)}\t{len(contents)}\t"
                    f"{digest_file(leak_path)}\n"
                )
        (result_dir / "leaked-summaries.tsv").write_text("".join(leak_lines))
        leaked_summary_count = len(leak_lines) - 1
        terminal_pass = terminal_outcome == "PASS"
        strict_single_pass = len(invocation_rows) == 1 and terminal_pass
        invocation_lines.append(
            f"{cell['test']}\t{cell['selector']}\t{repetition}\t"
            f"{0 if terminal_pass else 1}\t1\t{len(invocation_rows)}\t"
            f"{'yes' if strict_single_pass else 'no'}\t{leaked_summary_count}\t{result_dir}\n"
        )
        for row, kind in zip(invocation_rows, kinds, strict=True):
            host_artifact = result_dir / str(row["artifact_dir"])[len("/results/") :]
            for child in (
                "home",
                "xdg-config",
                "tmp",
                "fixtures",
                "recording",
                "workdir/1",
            ):
                (host_artifact / child).mkdir(parents=True, exist_ok=True)
            log_dir = host_artifact / "verify-logs" / "verify-1"
            log_dir.mkdir(parents=True)
            captures_dir = host_artifact / "captures"
            captures_dir.mkdir(parents=True)
            (captures_dir / "verify-1.stdout").write_bytes(
                str(row["attempts"][0]["stdout"]).encode()
            )
            (captures_dir / "verify-1.stderr").write_bytes(
                str(row["attempts"][0]["stderr"]).encode()
            )
            report_file = host_artifact / "verify-1.json"
            report_value = row["attempts"][0]["verification_report"]
            if row["attempts"][0]["status"] is None and row["attempts"][0]["signal"] is None:
                artifact_state = "pre-attempt-not-run"
                report_field = "-"
                run1_field = "-"
                run2_field = "-"
            elif report_value is None:
                artifact_state = "executed-no-report"
                report_field = "-"
                run1_field = "-"
                run2_field = "-"
            else:
                report_text = str(report_value)
                artifact_state = "executed-report"
                report_file.write_text(report_text)
                report_field = str(report_file)
                run1_log = log_dir / "run1_log_fixture"
                report_document = json.loads(report_text)
                compared_counts = report_document.get("compared_log_messages")
                left_count = (
                    compared_counts.get("left")
                    if isinstance(compared_counts, dict)
                    else None
                )
                right_count = (
                    compared_counts.get("right")
                    if isinstance(compared_counts, dict)
                    else None
                )
                run1_log.write_bytes(
                    b"" if left_count == 0 else b"one retained INFO record\n"
                )
                run1_field = str(run1_log)
                if report_document["verdict"] == "no_result":
                    if (
                        row["result"] == "timeout"
                        and kind == "timeout"
                        and cell["test"] == cells[5]["test"]
                    ):
                        if row["attempt"] == 1:
                            run1_log.unlink()
                            run1_field = "-"
                        else:
                            run1_log.unlink()
                            run1_field = "-"
                        run2_field = "-"
                    elif row["result"] == "timeout" and kind == "timeout_first_run":
                        if row["attempt"] == 1:
                            run2_log = log_dir / "run2_log_fixture"
                            run2_log.write_bytes(b"")
                            run2_field = str(run2_log)
                        else:
                            run2_field = "-"
                    elif (
                        row["result"] == "timeout"
                        and kind == "timeout"
                        and cell["test"] == cells[12]["test"]
                        and row["attempt"] == 2
                    ):
                        run2_log = log_dir / "run2_log_fixture"
                        run2_log.write_bytes(b"")
                        run2_field = str(run2_log)
                    else:
                        run2_field = "-"
                else:
                    run2_log = log_dir / "run2_log_fixture"
                    run2_log.write_bytes(
                        b"" if right_count == 0 else b"one retained INFO record\n"
                    )
                    run2_field = str(run2_log)
            report_staging_field = "-"
            if (
                (kind == "timeout_no_report" and row["attempt"] == 1)
                or (
                    kind == "timeout"
                    and cell["test"] == cells[5]["test"]
                    and row["attempt"] == 1
                )
            ):
                report_staging = host_artifact / ".tmpA1b2C3"
                report_staging.write_bytes(b"partial atomic report staging bytes\n")
                report_staging_field = str(report_staging)
            artifact_lines.append(
                f"{cell['test']}\t{repetition}\t{row['attempt']}\t{result_file}\t{digest_bytes(result_bytes)}\t"
                f"{row['artifact_dir']}\t{artifact_state}\t{report_field}\t"
                f"{row['attempts'][0]['verification_report_sha256'] or '-'}\t{run1_field}\t{run2_field}\t"
                f"{report_staging_field}\n"
            )
    (results_root / "invocations.tsv").write_text("".join(invocation_lines))
    (evidence / "all-results.jsonl").write_bytes(b"".join(aggregate))
    (evidence / "artifacts.tsv").write_text("".join(artifact_lines))
    rebuild_artifact_inventory(evidence)
    return evidence


def exercise_leaked_summary_discriminators(
    evidence: Path, cells: list[dict[str, str]]
) -> None:
    leak_cell = cells[8]
    slug = leak_cell["test"].replace("/", "-")
    result_dir = evidence / "results" / slug / "repetition-1"
    leak_dir = result_dir / "leaked-private-summaries"
    leak_manifest = result_dir / "leaked-summaries.tsv"
    invocation_ledger = evidence / "results/invocations.tsv"
    original_invocations = invocation_ledger.read_text()

    def set_leak_count(test_id: str, count: int) -> None:
        lines = original_invocations.splitlines(keepends=True)
        for index, line in enumerate(lines[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[0] == test_id and fields[2] == "1":
                fields[7] = str(count)
                lines[index] = "\t".join(fields) + "\n"
                invocation_ledger.write_text("".join(lines))
                return
        raise AssertionError("leaked-summary fixture invocation is absent")

    leak_paths = sorted(leak_dir.iterdir())
    require(
        len(leak_paths) == 2
        and {path.name for path in leak_paths}
        == {
            ".hermit-verify-summary-fixture-one",
            ".hermit-verify-summary-fixture-two",
        },
        "two leaked private summaries are attributed to their owning invocation",
    )

    original_manifest = leak_manifest.read_text()
    original_manifest_lines = original_manifest.splitlines(keepends=True)
    first_saved = evidence.parent / "coordinated-first-leak"
    second_saved = evidence.parent / "coordinated-second-leak"
    leak_paths[0].rename(first_saved)
    remaining_name = leak_paths[1].name
    remaining_line = next(
        line for line in original_manifest_lines[1:] if remaining_name in line
    )
    leak_manifest.write_text(original_manifest_lines[0] + remaining_line)
    set_leak_count(leak_cell["test"], 1)
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode == 0,
        "one signal-killed timeout leak remains complete preserved evidence",
    )
    leak_paths[1].rename(second_saved)
    leak_manifest.write_text(original_manifest_lines[0])
    set_leak_count(leak_cell["test"], 0)
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode == 0,
        "signal-killed timeout may legitimately leave no private summary",
    )
    first_saved.rename(leak_paths[0])
    second_saved.rename(leak_paths[1])
    leak_manifest.write_text(original_manifest)
    invocation_ledger.write_text(original_invocations)
    rebuild_artifact_inventory(evidence)

    saved_leak = evidence.parent / "temporarily-removed-leak"
    leak_paths[0].rename(saved_leak)
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "disappearance of an attributed private summary fails closed",
    )
    saved_leak.rename(leak_paths[0])
    rebuild_artifact_inventory(evidence)

    unledgered = leak_dir / ".hermit-verify-summary-unledgered"
    unledgered.write_text("unattributed summary\n")
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "an unledgered private summary addition fails closed",
    )
    unledgered.unlink()
    rebuild_artifact_inventory(evidence)

    manifest_lines = original_manifest.splitlines(keepends=True)
    fields = manifest_lines[1].rstrip("\n").split("\t")
    fields[4] = "0" * 64
    manifest_lines[1] = "\t".join(fields) + "\n"
    leak_manifest.write_text("".join(manifest_lines))
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "a leaked-summary ledger hash mismatch fails closed",
    )
    leak_manifest.write_text(original_manifest)
    rebuild_artifact_inventory(evidence)

    impossible_kind = original_manifest.splitlines(keepends=True)
    impossible_fields = impossible_kind[1].rstrip("\n").split("\t")
    impossible_fields[0] = "backend-engagement"
    impossible_kind[1] = "\t".join(impossible_fields) + "\n"
    leak_manifest.write_text("".join(impossible_kind))
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "KVM evidence rejects an impossible backend-engagement summary leak",
    )
    leak_manifest.write_text(original_manifest)

    over_count = leak_dir / ".hermit-verify-summary-fixture-three"
    over_count.write_text("third verify summary fixture\n")
    relative = over_count.relative_to(evidence)
    over_manifest = original_manifest + (
        f"verify\t{relative}\t{int(over_count.stat().st_mtime)}\t"
        f"{over_count.stat().st_size}\t{digest_file(over_count)}\n"
    )
    leak_manifest.write_text(over_manifest)
    set_leak_count(leak_cell["test"], 3)
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "leaked-summary count cannot exceed the signal-killed attempt count",
    )
    over_count.unlink()
    leak_manifest.write_text(original_manifest)
    invocation_ledger.write_text(original_invocations)
    rebuild_artifact_inventory(evidence)

    # A private summary is created only after both retained-log temp files.
    # Pending signal timeouts with zero or one retained log cannot account for
    # a leak even though their timeout disposition alone is otherwise valid.
    partial_cell = cells[5]
    partial_slug = partial_cell["test"].replace("/", "-")
    partial_result_dir = evidence / "results" / partial_slug / "repetition-1"
    partial_leak_dir = partial_result_dir / "leaked-private-summaries"
    partial_leak_manifest = partial_result_dir / "leaked-summaries.tsv"
    partial_leak = partial_leak_dir / ".hermit-verify-summary-impossible-partial"
    partial_leak.write_text("impossible partial-log summary\n")
    partial_manifest_original = partial_leak_manifest.read_text()
    partial_leak_manifest.write_text(
        partial_manifest_original
        + f"verify\t{partial_leak.relative_to(evidence)}\t"
        f"{int(partial_leak.stat().st_mtime)}\t{partial_leak.stat().st_size}\t"
        f"{digest_file(partial_leak)}\n"
    )
    set_leak_count(partial_cell["test"], 1)
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "signal-killed timeout with zero retained logs cannot account for a leak",
    )

    artifact_ledger = evidence / "artifacts.tsv"
    artifact_ledger_original = artifact_ledger.read_text()
    artifact_lines = artifact_ledger_original.splitlines(keepends=True)
    partial_run1 = None
    for index, line in enumerate(artifact_lines[1:], start=1):
        fields = line.rstrip("\n").split("\t")
        if fields[0] == partial_cell["test"] and fields[1:3] == ["1", "1"]:
            host_artifact = partial_result_dir / fields[5].removeprefix("/results/")
            partial_run1 = host_artifact / "verify-logs/verify-1/run1_log_partial"
            partial_run1.write_bytes(b"partial retained log\n")
            fields[9] = str(partial_run1)
            artifact_lines[index] = "\t".join(fields) + "\n"
            break
    require(partial_run1 is not None, "partial-log leak discriminator found its attempt")
    artifact_ledger.write_text("".join(artifact_lines))
    rebuild_artifact_inventory(evidence)
    require(
        validate_evidence(evidence).returncode != 0,
        "signal-killed timeout with only run1 cannot account for a leak",
    )
    partial_run1.unlink()
    partial_leak.unlink()
    partial_leak_manifest.write_text(partial_manifest_original)
    artifact_ledger.write_text(artifact_ledger_original)
    invocation_ledger.write_text(original_invocations)
    rebuild_artifact_inventory(evidence)


def main() -> int:
    semantic_only = sys.argv[1:] == ["--semantic-only"]
    leak_only = sys.argv[1:] == ["--leak-only"]
    rows_only = sys.argv[1:] == ["--rows-only"]
    launcher_only = sys.argv[1:] == ["--launcher-only"]
    if sys.argv[1:] and not (
        semantic_only or leak_only or rows_only or launcher_only
    ):
        raise SystemExit(
            "usage: self-test.py "
            "[--semantic-only|--leak-only|--rows-only|--launcher-only]"
        )
    cells: list[dict[str, str]] = json.loads(EXPECTED.read_text())
    kernel = os.uname().release
    target = cells[0]
    source_contracts = source_guest_contracts()
    frozen_contracts = {
        cell["test"]: {
            "guest_argv_template": cell.get("guest_argv_template"),
            "test_sha256": cell.get("test_sha256"),
        }
        for cell in cells
    }
    require(
        len(cells) == 119
        and len(frozen_contracts) == 119
        and set(frozen_contracts) <= set(source_contracts)
        and all(
            frozen_contracts[test_id] == source_contracts[test_id]
            for test_id in frozen_contracts
        ),
        "all 119 frozen guest argv templates and test digests regenerate exactly from source YAML/files",
    )

    if leak_only:
        with tempfile.TemporaryDirectory(prefix="kvm-ratchet-90-leak-test-") as temporary:
            evidence = materialize_evidence_fixture(Path(temporary), cells, kernel)
            require(
                validate_evidence(evidence).returncode == 0,
                "two-leak fixture is complete evidence",
            )
            exercise_leaked_summary_discriminators(evidence, cells)
        print("PASS no build/Hermit/KVM/systemd action was executed")
        return 0

    real_pass_shape = make_row(target, 1, 1, "pass", kernel=kernel)
    real_pass_report = json.loads(real_pass_shape["attempts"][0]["verification_report"])
    require(
        "no_result_reason" not in real_pass_report
        and isinstance(real_pass_report.get("runtime"), dict),
        "matched fixture uses current sparse producer bytes with typed runtime",
    )

    zero_info = make_row(target, 1, 1, "pass", kernel=kernel, info_messages=0)
    require(not jq_eligible([zero_info], target, kernel), "zero-INFO PASS cannot advance")

    for malformed_count, label in (
        ("3", "string"),
        ({"count": 3}, "object"),
        (1.5, "fractional"),
    ):
        wrong_info_type = make_row(
            target, 1, 1, "pass", kernel=kernel, info_messages=malformed_count
        )
        require(
            not jq_eligible([wrong_info_type], target, kernel),
            f"{label} compared-INFO count cannot advance",
        )

    unequal_match = make_row(target, 1, 1, "pass", kernel=kernel)
    unequal_match_report = json.loads(
        unequal_match["attempts"][0]["verification_report"]
    )
    unequal_match_report["compared_log_messages"]["right"] = 4
    unequal_match["attempts"][0]["verification_report"] = serialize_disk_report(
        unequal_match_report
    )
    unequal_match["attempts"][0]["verification_report_sha256"] = digest_bytes(
        unequal_match["attempts"][0]["verification_report"].encode()
    )
    require(
        not jq_eligible([unequal_match], target, kernel),
        "matched canonical PASS rejects unequal compared-INFO counts",
    )

    malformed = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    malformed["attempts"][0]["verification_report"] = "{"
    require(not jq_eligible([malformed], target, kernel), "malformed report cannot advance")

    missing_report_lf = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    missing_report_lf["attempts"][0]["verification_report"] = missing_report_lf[
        "attempts"
    ][0]["verification_report"].removesuffix("\n")
    missing_report_lf["attempts"][0]["verification_report_sha256"] = digest_bytes(
        missing_report_lf["attempts"][0]["verification_report"].encode()
    )
    require(
        not jq_eligible([missing_report_lf], target, kernel),
        "disk-backed report serialization requires its single final LF",
    )

    for field in (
        "failure_class",
        "error_kind",
        "reason",
        "runtime",
        "execution_path",
        "diversity",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ):
        missing_row_field = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
        del missing_row_field[field]
        require(
            not jq_eligible([missing_row_field], target, kernel),
            f"strict PASS missing row {field} is rejected",
        )
    for field in (
        "error_kind",
        "status",
        "signal",
        "timed_out",
        "reason",
        "runtime",
        "observation_sha256",
        "sabre_path_evidence",
        "sabre_path_evidence_sha256",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ):
        missing_attempt_field = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
        del missing_attempt_field["attempts"][0][field]
        require(
            not jq_eligible([missing_attempt_field], target, kernel),
            f"strict PASS missing attempt {field} is rejected",
        )
    for field in (
        "infrastructure_error",
        "guest_exit_code",
        "guest_signal",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ):
        missing_report_field = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
        report_document = json.loads(
            missing_report_field["attempts"][0]["verification_report"]
        )
        del report_document[field]
        missing_report_field["attempts"][0][
            "verification_report"
        ] = serialize_disk_report(report_document)
        missing_report_field["attempts"][0]["verification_report_sha256"] = digest_bytes(
            missing_report_field["attempts"][0]["verification_report"].encode()
        )
        require(
            not jq_eligible([missing_report_field], target, kernel),
            f"strict PASS missing report {field} is rejected",
        )

    missing_optional_runtime = deepcopy(
        make_row(target, 1, 1, "pass", kernel=kernel)
    )
    missing_optional_runtime_report = json.loads(
        missing_optional_runtime["attempts"][0]["verification_report"]
    )
    del missing_optional_runtime_report["runtime"]
    missing_optional_runtime["runtime"] = None
    missing_optional_runtime["attempts"][0]["runtime"] = None
    missing_optional_runtime["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(missing_optional_runtime_report)
    missing_optional_runtime["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        missing_optional_runtime["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_eligible([missing_optional_runtime], target, kernel),
        "matched producer report may omit unavailable runtime while row and attempt are null",
    )

    explicit_null_no_result = deepcopy(
        make_row(target, 1, 1, "pass", kernel=kernel)
    )
    explicit_null_no_result_report = json.loads(
        explicit_null_no_result["attempts"][0]["verification_report"]
    )
    explicit_null_no_result_report["no_result_reason"] = None
    explicit_null_no_result["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(explicit_null_no_result_report)
    explicit_null_no_result["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        explicit_null_no_result["attempts"][0]["verification_report"].encode()
    )
    require(
        not jq_eligible([explicit_null_no_result], target, kernel),
        "matched producer report requires omitted, not explicit-null, no_result_reason",
    )

    forged_dbt_report = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    forged_dbt_document = json.loads(
        forged_dbt_report["attempts"][0]["verification_report"]
    )
    forged_dbt_document["dbt_counted_branches"] = {"left": 1, "right": 1}
    forged_dbt_report["attempts"][0]["verification_report"] = serialize_disk_report(
        forged_dbt_document
    )
    forged_dbt_report["attempts"][0]["verification_report_sha256"] = digest_bytes(
        forged_dbt_report["attempts"][0]["verification_report"].encode()
    )
    require(
        not jq_eligible([forged_dbt_report], target, kernel),
        "KVM verification report rejects forged DBT branch evidence",
    )

    for malformed_runtime, label in (
        ("runtime", "scalar"),
        ({"run1": {"scheduler_turns": 1, "virtual_nanoseconds": -1}}, "negative"),
        ({"run1": {"scheduler_turns": 1.5, "virtual_nanoseconds": 2}}, "fractional"),
        (
            {
                "run1": {
                    "scheduler_turns": 1,
                    "virtual_nanoseconds": 2,
                    "unknown": 3,
                }
            },
            "unknown-key",
        ),
    ):
        malformed_runtime_row = deepcopy(
            make_row(target, 1, 1, "pass", kernel=kernel)
        )
        malformed_runtime_report = json.loads(
            malformed_runtime_row["attempts"][0]["verification_report"]
        )
        malformed_runtime_report["runtime"] = malformed_runtime
        malformed_runtime_row["runtime"] = deepcopy(malformed_runtime)
        malformed_runtime_row["attempts"][0]["runtime"] = deepcopy(
            malformed_runtime
        )
        malformed_runtime_row["attempts"][0][
            "verification_report"
        ] = serialize_disk_report(malformed_runtime_report)
        malformed_runtime_row["attempts"][0][
            "verification_report_sha256"
        ] = digest_bytes(
            malformed_runtime_row["attempts"][0]["verification_report"].encode()
        )
        require(
            not jq_eligible([malformed_runtime_row], target, kernel),
            f"{label} verification runtime fails closed",
        )

    optional_syscalls_runtime = deepcopy(
        make_row(target, 1, 1, "pass", kernel=kernel)
    )
    optional_syscalls_report = json.loads(
        optional_syscalls_runtime["attempts"][0]["verification_report"]
    )
    for runtime_copy in (
        optional_syscalls_report["runtime"],
        optional_syscalls_runtime["runtime"],
        optional_syscalls_runtime["attempts"][0]["runtime"],
    ):
        del runtime_copy["run1"]["syscalls"]
        del runtime_copy["run2"]["syscalls"]
    optional_syscalls_runtime["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(optional_syscalls_report)
    optional_syscalls_runtime["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        optional_syscalls_runtime["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_eligible([optional_syscalls_runtime], target, kernel),
        "typed runtime accepts producer-optional syscall counters",
    )

    mismatched_pass_coordinate = deepcopy(
        make_row(target, 1, 1, "pass", kernel=kernel)
    )
    mismatched_pass_coordinate["attempts"][0]["first_divergent_record"] = 1
    require(
        not jq_eligible([mismatched_pass_coordinate], target, kernel),
        "strict PASS rejects a contradictory attempt-level divergence coordinate",
    )

    mismatched_runtime = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    mismatched_runtime["runtime"] = {"run1": {"scheduler_turns": 1}}
    require(
        not jq_eligible([mismatched_runtime], target, kernel),
        "row, attempt, and verification-report runtime must agree",
    )
    impossible_observation = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    impossible_observation["attempts"][0]["observation_sha256"] = "not-a-digest"
    require(
        not jq_eligible([impossible_observation], target, kernel),
        "verify/KVM row rejects incompatible observation evidence",
    )
    short_row_duration = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    short_row_duration["duration_ms"] = 0
    require(
        not jq_eligible([short_row_duration], target, kernel),
        "cell duration cannot be shorter than its attempt duration",
    )

    malformed_timeout_policy = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    malformed_timeout_policy["execution_cpu_timeout_seconds"] = "22"
    require(
        not jq_eligible([malformed_timeout_policy], target, kernel),
        "string execution timeout cannot advance",
    )
    fractional_timeout_policy = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    fractional_timeout_policy["execution_wall_timeout_seconds"] = 57.5
    fractional_timeout_policy["timeout_seconds"] = 57.5
    require(
        not jq_eligible([fractional_timeout_policy], target, kernel),
        "fractional execution timeout cannot advance",
    )
    widened_timeout_policy = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    widened_timeout_policy["execution_cpu_timeout_seconds"] = 23
    widened_timeout_policy["execution_wall_timeout_seconds"] = 58
    widened_timeout_policy["timeout_seconds"] = 58
    require(
        not jq_eligible([widened_timeout_policy], target, kernel),
        "widened 23/58 timeout policy cannot move the frozen goalpost",
    )

    flipped = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    flipped["classification"] = (
        "required" if flipped["classification"] == "disabled" else "disabled"
    )
    require(not jq_eligible([flipped], target, kernel), "flipped disabled-cell classification is rejected")
    manual_target = next(cell for cell in cells if cell["selector"] == "--include-manual")
    flipped_manual = deepcopy(make_row(manual_target, 1, 1, "pass", kernel=kernel))
    flipped_manual["classification"] = "disabled"
    require(
        not jq_eligible([flipped_manual], manual_target, kernel),
        "flipped enabled-red classification is rejected",
    )

    relaxed = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    separator = relaxed["argv"].index("--")
    relaxed["argv"].insert(separator, "--no-virtualize-time")
    sync_invocation_argv(relaxed)
    require(not jq_eligible([relaxed], target, kernel), "forbidden relaxation argv is rejected")

    missing_base_env = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    missing_base_env["argv"].remove("--base-env=minimal")
    sync_invocation_argv(missing_base_env)
    require(
        not jq_eligible([missing_base_env], target, kernel),
        "fixed invocation requires exactly one --base-env=minimal",
    )
    widened_verify_allow = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    widened_verify_allow["argv"].insert(
        widened_verify_allow["argv"].index("--"), "--verify-allow=both"
    )
    sync_invocation_argv(widened_verify_allow)
    require(
        not jq_eligible([widened_verify_allow], target, kernel),
        "verify-allow override cannot widen the fixed Success-only command",
    )
    duplicate_backend = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    duplicate_backend["argv"][duplicate_backend["argv"].index("--"):duplicate_backend["argv"].index("--")] = [
        "--backend",
        "kvm",
    ]
    sync_invocation_argv(duplicate_backend)
    require(
        not jq_eligible([duplicate_backend], target, kernel),
        "backend is bound by one exact --backend kvm pair",
    )

    moved_strict = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    moved_strict["argv"].remove("--strict")
    delimiter = moved_strict["argv"].index("--")
    moved_strict["argv"].insert(delimiter + 1, "--strict")
    moved_strict["guest_argv"].insert(0, "--strict")
    moved_strict["attempts"][0]["guest_argv"] = deepcopy(
        moved_strict["guest_argv"]
    )
    sync_invocation_argv(moved_strict)
    require(
        not jq_eligible([moved_strict], target, kernel),
        "producer-owned strict flag cannot be moved into the guest argv",
    )

    missing_verify_json = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    verify_json_index = missing_verify_json["argv"].index("--verify-json")
    del missing_verify_json["argv"][verify_json_index : verify_json_index + 2]
    sync_invocation_argv(missing_verify_json)
    require(
        not jq_eligible([missing_verify_json], target, kernel),
        "fixed invocation requires the producer-owned verify-json pair",
    )

    substituted_verify_json = deepcopy(
        make_row(target, 1, 1, "pass", kernel=kernel)
    )
    verify_json_index = substituted_verify_json["argv"].index("--verify-json")
    substituted_verify_json["argv"][verify_json_index + 1] = "/results/other.json"
    sync_invocation_argv(substituted_verify_json)
    require(
        not jq_eligible([substituted_verify_json], target, kernel),
        "verify-json path is derived exactly from the row artifact directory",
    )

    missing_keep_logs = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    missing_keep_logs["argv"].remove("--keep-logs")
    sync_invocation_argv(missing_keep_logs)
    require(
        not jq_eligible([missing_keep_logs], target, kernel),
        "fixed invocation requires retained verify logs",
    )

    substituted_log_dir = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    log_dir_index = substituted_log_dir["argv"].index("--verify-log-dir")
    substituted_log_dir["argv"][log_dir_index + 1] = "/results/other-logs"
    sync_invocation_argv(substituted_log_dir)
    require(
        not jq_eligible([substituted_log_dir], target, kernel),
        "verify-log-dir path is derived exactly from the row artifact directory",
    )

    reordered_verify = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    verify_index = reordered_verify["argv"].index("--verify")
    strict_index = reordered_verify["argv"].index("--verify-strict")
    reordered_verify["argv"][verify_index], reordered_verify["argv"][strict_index] = (
        reordered_verify["argv"][strict_index],
        reordered_verify["argv"][verify_index],
    )
    sync_invocation_argv(reordered_verify)
    require(
        not jq_eligible([reordered_verify], target, kernel),
        "producer-owned option order is exact",
    )

    duplicate_delimiter = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    duplicate_delimiter["argv"].insert(duplicate_delimiter["argv"].index("--"), "--")
    sync_invocation_argv(duplicate_delimiter)
    require(
        not jq_eligible([duplicate_delimiter], target, kernel),
        "fixed invocation contains exactly one option/guest delimiter",
    )

    mismatched_guest_tail = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    mismatched_guest_tail["guest_argv"] = ["/test/forged-program"]
    mismatched_guest_tail["attempts"][0]["guest_argv"] = deepcopy(
        mismatched_guest_tail["guest_argv"]
    )
    delimiter = mismatched_guest_tail["argv"].index("--")
    mismatched_guest_tail["argv"] = (
        mismatched_guest_tail["argv"][: delimiter + 1]
        + mismatched_guest_tail["guest_argv"]
    )
    sync_invocation_argv(mismatched_guest_tail)
    mismatched_guest_tail["shell_command"] = producer_shell_command(
        mismatched_guest_tail["cwd"],
        mismatched_guest_tail["env"],
        mismatched_guest_tail["argv"],
    )
    mismatched_guest_tail["attempts"][0]["shell_command"] = (
        mismatched_guest_tail["shell_command"]
    )
    require(
        not jq_eligible([mismatched_guest_tail], target, kernel),
        "coordinated wrong guest argv cannot replace the frozen test command",
    )

    wrong_test_digest = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    wrong_test_digest["test_sha256"] = "0" * 64
    require(
        not jq_eligible([wrong_test_digest], target, kernel),
        "test digest is bound to the frozen source-derived recipe digest",
    )

    forged_shell_command = deepcopy(make_row(target, 1, 1, "pass", kernel=kernel))
    forged_shell_command["shell_command"] = "cd /src && env forged-command"
    forged_shell_command["attempts"][0]["shell_command"] = (
        forged_shell_command["shell_command"]
    )
    require(
        not jq_eligible([forged_shell_command], target, kernel),
        "coordinated row/attempt shell command must equal the producer rendering",
    )

    retry_rows = [
        make_row(target, 1, 1, "divergence", kernel=kernel),
        make_row(target, 1, 2, "pass", kernel=kernel),
    ]
    require(not jq_eligible(retry_rows, target, kernel), "failure then retry PASS cannot advance")

    all_timeout = excluded_round1_rows(cells, kernel)
    second_test = cells[1]["test"]
    all_timeout = without_invocation(all_timeout, second_test, 1)
    all_timeout.extend(
        [
            make_row(cells[1], 1, 1, "divergence", kernel=kernel),
            make_row(cells[1], 1, 2, "divergence", kernel=kernel),
        ]
    )
    typed = jq_validate(all_timeout, kernel)
    require(typed["ok"] is True, "typed divergence/timeouts are evidence-valid exclusions")
    require(typed["provisional_qualified_cell_count"] == 0, "typed exclusions do not qualify")
    require(typed["killed_timeout_rows"] == 236, "common killed timeouts retain their exact typed shape")
    zero_side_divergence = deepcopy(all_timeout)
    for row in zero_side_divergence:
        if row["test"] == second_test:
            divergence_report = json.loads(row["attempts"][0]["verification_report"])
            divergence_report["compared_log_messages"]["left"] = 0
            row["attempts"][0]["verification_report"] = serialize_disk_report(
                divergence_report
            )
            row["attempts"][0]["verification_report_sha256"] = digest_bytes(
                row["attempts"][0]["verification_report"].encode()
            )
    require(
        jq_validate(zero_side_divergence, kernel)["ok"] is False,
        "zero-sided divergence is incomplete canonical evidence",
    )
    impossible_divergence_status = deepcopy(all_timeout)
    for row in impossible_divergence_status:
        if row["test"] == second_test:
            row["attempts"][0]["status"] = 7
    require(
        jq_validate(impossible_divergence_status, kernel)["ok"] is False,
        "non-timeout canonical divergence requires outer status 1",
    )
    impossible_divergence_guest = deepcopy(all_timeout)
    for row in impossible_divergence_guest:
        if row["test"] == second_test:
            report_document = json.loads(row["attempts"][0]["verification_report"])
            report_document["guest_exit_code"] = None
            row["attempts"][0]["verification_report"] = serialize_disk_report(
                report_document
            )
            row["attempts"][0]["verification_report_sha256"] = digest_bytes(
                row["attempts"][0]["verification_report"].encode()
            )
    require(
        jq_validate(impossible_divergence_guest, kernel)["ok"] is False,
        "canonical divergence requires a completed run-2 guest disposition",
    )
    nonzero_divergence_guest = deepcopy(all_timeout)
    for row in nonzero_divergence_guest:
        if row["test"] == second_test:
            report_document = json.loads(row["attempts"][0]["verification_report"])
            report_document["guest_exit_code"] = 7
            row["attempts"][0]["verification_report"] = serialize_disk_report(
                report_document
            )
            row["attempts"][0]["verification_report_sha256"] = digest_bytes(
                row["attempts"][0]["verification_report"].encode()
            )
    require(
        jq_validate(nonzero_divergence_guest, kernel)["ok"] is True,
        "run-2 nonzero guest disposition remains valid canonical divergence",
    )
    mismatched_attempt_divergence = deepcopy(all_timeout)
    mismatched_attempt_row = next(
        row
        for row in mismatched_attempt_divergence
        if row["test"] == second_test
    )
    mismatched_attempt_row["attempts"][0]["first_divergent_record"] = 8
    require(
        jq_validate(mismatched_attempt_divergence, kernel)["ok"] is False,
        "attempt-level divergence coordinates must match row and report",
    )
    missing_attempt_divergence = deepcopy(all_timeout)
    missing_attempt_row = next(
        row for row in missing_attempt_divergence if row["test"] == second_test
    )
    del missing_attempt_row["attempts"][0]["first_divergent_record"]
    require(
        jq_validate(missing_attempt_divergence, kernel)["ok"] is False,
        "missing attempt-level divergence coordinate fails closed",
    )

    crash_test = cells[2]
    crash_campaign = without_invocation(all_timeout, crash_test["test"], 1)
    crash_campaign.extend(
        [
            make_row(crash_test, 1, 1, "crash", kernel=kernel),
            make_row(crash_test, 1, 2, "crash", kernel=kernel),
        ]
    )
    crash_summary = jq_validate(crash_campaign, kernel)
    require(crash_summary["ok"] is True, "typed first-run-rejected crash is evidence-valid")
    require(crash_summary["crash_error_rows"] == 2, "crash attempts remain explicit excluded rows")
    crash_run1_runtime = deepcopy(crash_campaign)
    crash_run1_runtime_row = next(
        row for row in crash_run1_runtime if row["test"] == crash_test["test"]
    )
    crash_run1_runtime_report = json.loads(
        crash_run1_runtime_row["attempts"][0]["verification_report"]
    )
    run1_runtime = {"run1": canonical_runtime()["run1"]}
    crash_run1_runtime_report["runtime"] = deepcopy(run1_runtime)
    crash_run1_runtime_row["runtime"] = deepcopy(run1_runtime)
    crash_run1_runtime_row["attempts"][0]["runtime"] = deepcopy(run1_runtime)
    crash_run1_runtime_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(crash_run1_runtime_report)
    crash_run1_runtime_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        crash_run1_runtime_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(crash_run1_runtime, kernel)["ok"] is True,
        "first-run-rejected report may retain typed run1-only runtime",
    )
    for malformed_runtime, label in (
        ({"run2": canonical_runtime()["run2"]}, "run2-only"),
        (canonical_runtime(), "run1-and-run2"),
    ):
        malformed_crash_runtime = deepcopy(crash_campaign)
        malformed_crash_runtime_row = next(
            row
            for row in malformed_crash_runtime
            if row["test"] == crash_test["test"]
        )
        malformed_crash_runtime_report = json.loads(
            malformed_crash_runtime_row["attempts"][0]["verification_report"]
        )
        malformed_crash_runtime_report["runtime"] = deepcopy(malformed_runtime)
        malformed_crash_runtime_row["runtime"] = deepcopy(malformed_runtime)
        malformed_crash_runtime_row["attempts"][0]["runtime"] = deepcopy(
            malformed_runtime
        )
        malformed_crash_runtime_row["attempts"][0][
            "verification_report"
        ] = serialize_disk_report(malformed_crash_runtime_report)
        malformed_crash_runtime_row["attempts"][0][
            "verification_report_sha256"
        ] = digest_bytes(
            malformed_crash_runtime_row["attempts"][0][
                "verification_report"
            ].encode()
        )
        require(
            jq_validate(malformed_crash_runtime, kernel)["ok"] is False,
            f"first-run-rejected report rejects {label} runtime",
        )
    impossible_crash_status = deepcopy(crash_campaign)
    crash_status_row = next(
        row for row in impossible_crash_status if row["test"] == crash_test["test"]
    )
    crash_status_row["attempts"][0]["status"] = 7
    require(
        jq_validate(impossible_crash_status, kernel)["ok"] is False,
        "ordinary first-run rejection requires outer status 125",
    )
    malformed_crash = deepcopy(crash_campaign)
    crash_row = next(row for row in malformed_crash if row["test"] == crash_test["test"])
    crash_report = json.loads(crash_row["attempts"][0]["verification_report"])
    crash_report["no_result_reason"]["exit_code"] = 2
    crash_row["attempts"][0]["verification_report"] = serialize_disk_report(
        crash_report
    )
    crash_row["attempts"][0]["verification_report_sha256"] = digest_bytes(
        crash_row["attempts"][0]["verification_report"].encode()
    )
    require(jq_validate(malformed_crash, kernel)["ok"] is False, "mismatched crash disposition fails closed")
    malformed_crash_bytes = deepcopy(crash_campaign)
    crash_bytes_row = next(
        row for row in malformed_crash_bytes if row["test"] == crash_test["test"]
    )
    crash_bytes_report = json.loads(crash_bytes_row["attempts"][0]["verification_report"])
    crash_bytes_report["no_result_reason"]["stderr_bytes"] = "17"
    crash_bytes_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(crash_bytes_report)
    require(
        jq_validate(malformed_crash_bytes, kernel)["ok"] is False,
        "string no-result byte count fails closed",
    )

    timeout_shapes = (
        (cells[5], "timeout", 3, "live-timeout pending NotRun report"),
        (cells[6], "timeout_first_run", 3, "post-exit first-run-rejected CPU timeout"),
        (cells[7], "timeout_divergence", 3, "post-exit divergent CPU timeout"),
        (cells[8], "timeout_signaled_match", 3, "live-timeout matched report"),
        (
            cells[9],
            "timeout_left_zero_divergence",
            3,
            "zero-left-count divergent CPU timeout",
        ),
        (cells[10], "timeout_zero_divergence", 3, "zero/zero output-divergent CPU timeout"),
        (cells[11], "timeout_no_report", 3, "executed killed timeout without report"),
        (cells[13], "timeout_zero_match", 3, "zero/zero matched CPU timeout"),
    )
    timeout_campaigns: dict[str, list[dict[str, object]]] = {}
    for timeout_cell, timeout_kind, count, label in timeout_shapes:
        campaign_rows = replace_round1_invocation(
            all_timeout,
            timeout_cell,
            timeout_kind,
            kernel,
            info_messages=count,
        )
        timeout_campaigns[timeout_kind] = campaign_rows
        timeout_summary = jq_validate(campaign_rows, kernel)
        require(timeout_summary["ok"] is True, f"{label} is valid excluded evidence")
        require(
            timeout_summary["provisional_qualified_cell_count"] == 0,
            f"{label} never qualifies",
        )

    pending_report_shape = json.loads(
        next(
            row
            for row in timeout_campaigns["timeout"]
            if row["test"] == cells[5]["test"]
        )["attempts"][0]["verification_report"]
    )
    require(
        "no_result_reason" in pending_report_shape
        and "runtime" not in pending_report_shape,
        "pending NotRun fixture uses current sparse producer bytes",
    )
    explicit_null_pending_runtime = deepcopy(timeout_campaigns["timeout"])
    explicit_null_pending_row = next(
        row
        for row in explicit_null_pending_runtime
        if row["test"] == cells[5]["test"]
    )
    explicit_null_pending_report = json.loads(
        explicit_null_pending_row["attempts"][0]["verification_report"]
    )
    explicit_null_pending_report["runtime"] = None
    explicit_null_pending_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(explicit_null_pending_report)
    explicit_null_pending_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        explicit_null_pending_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(explicit_null_pending_runtime, kernel)["ok"] is False,
        "absent runtime cannot be forged as explicit null in a pending report",
    )
    invented_pending_runtime = deepcopy(timeout_campaigns["timeout"])
    invented_pending_runtime_row = next(
        row
        for row in invented_pending_runtime
        if row["test"] == cells[5]["test"]
    )
    invented_pending_runtime_report = json.loads(
        invented_pending_runtime_row["attempts"][0]["verification_report"]
    )
    invented_pending_runtime_report["runtime"] = canonical_runtime()
    invented_pending_runtime_row["runtime"] = canonical_runtime()
    invented_pending_runtime_row["attempts"][0]["runtime"] = canonical_runtime()
    invented_pending_runtime_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(invented_pending_runtime_report)
    invented_pending_runtime_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        invented_pending_runtime_row["attempts"][0][
            "verification_report"
        ].encode()
    )
    require(
        jq_validate(invented_pending_runtime, kernel)["ok"] is False,
        "pending NotRun report rejects coordinated invented runtime",
    )

    for timeout_kind, timeout_cell, wrong_outcome, label in (
        ("timeout", cells[5], "FAIL", "pending NotRun"),
        ("timeout_first_run", cells[6], "FAIL", "first-run-rejected"),
        ("timeout_divergence", cells[7], "ERROR", "positive divergence"),
        ("timeout_signaled_match", cells[8], "ERROR", "positive match"),
        ("timeout_no_report", cells[11], "FAIL", "null-report"),
    ):
        wrong_timeout_outcome = deepcopy(timeout_campaigns[timeout_kind])
        wrong_timeout_row = next(
            row
            for row in wrong_timeout_outcome
            if row["test"] == timeout_cell["test"]
        )
        wrong_timeout_row["outcome"] = wrong_outcome
        wrong_timeout_row["attempts"][0]["outcome"] = wrong_outcome
        require(
            jq_validate(wrong_timeout_outcome, kernel)["ok"] is False,
            f"{label} timeout rejects outcome {wrong_outcome}",
        )

    zero_record_divergence = deepcopy(
        timeout_campaigns["timeout_left_zero_divergence"]
    )
    zero_record_row = next(
        row
        for row in zero_record_divergence
        if row["test"] == cells[9]["test"]
    )
    zero_record_report = json.loads(
        zero_record_row["attempts"][0]["verification_report"]
    )
    zero_record_report["first_divergent_record"] = 0
    zero_record_row["first_divergent_record"] = 0
    zero_record_row["attempts"][0]["first_divergent_record"] = 0
    zero_record_row["attempts"][0]["verification_report"] = serialize_disk_report(
        zero_record_report
    )
    zero_record_row["attempts"][0]["verification_report_sha256"] = digest_bytes(
        zero_record_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(zero_record_divergence, kernel)["ok"] is False,
        "first divergent record is one-based and rejects zero",
    )

    missing_asymmetric_message = deepcopy(
        timeout_campaigns["timeout_left_zero_divergence"]
    )
    missing_asymmetric_row = next(
        row
        for row in missing_asymmetric_message
        if row["test"] == cells[9]["test"]
    )
    missing_asymmetric_report = json.loads(
        missing_asymmetric_row["attempts"][0]["verification_report"]
    )
    missing_asymmetric_report["first_divergent_right_message"] = None
    missing_asymmetric_row["first_divergent_right_message"] = None
    missing_asymmetric_row["attempts"][0]["first_divergent_right_message"] = None
    missing_asymmetric_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(missing_asymmetric_report)
    missing_asymmetric_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        missing_asymmetric_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(missing_asymmetric_message, kernel)["ok"] is False,
        "zero-left divergence requires the retained right-side message",
    )

    right_zero_divergence = deepcopy(
        timeout_campaigns["timeout_left_zero_divergence"]
    )
    for row in right_zero_divergence:
        if row["test"] == cells[9]["test"]:
            right_zero_report = json.loads(
                row["attempts"][0]["verification_report"]
            )
            right_zero_report["compared_log_messages"] = {"left": 3, "right": 0}
            right_zero_report["first_divergent_left_message"] = (
                "INFO left-only record"
            )
            right_zero_report["first_divergent_right_message"] = None
            row["first_divergent_left_message"] = "INFO left-only record"
            row["first_divergent_right_message"] = None
            row["attempts"][0]["first_divergent_left_message"] = (
                "INFO left-only record"
            )
            row["attempts"][0]["first_divergent_right_message"] = None
            row["attempts"][0]["verification_report"] = serialize_disk_report(
                right_zero_report
            )
            row["attempts"][0]["verification_report_sha256"] = digest_bytes(
                row["attempts"][0]["verification_report"].encode()
            )
    require(
        jq_validate(right_zero_divergence, kernel)["ok"] is True,
        "positive-left/zero-right divergent timeout is valid excluded evidence",
    )

    orphan_virtual_time = deepcopy(timeout_campaigns["timeout_divergence"])
    orphan_virtual_time_row = next(
        row for row in orphan_virtual_time if row["test"] == cells[7]["test"]
    )
    orphan_virtual_time_report = json.loads(
        orphan_virtual_time_row["attempts"][0]["verification_report"]
    )
    orphan_virtual_time_report["first_divergent_virtual_nanoseconds"] = 0
    orphan_virtual_time_row["first_divergent_virtual_nanoseconds"] = 0
    orphan_virtual_time_row["attempts"][0][
        "first_divergent_virtual_nanoseconds"
    ] = 0
    orphan_virtual_time_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(orphan_virtual_time_report)
    orphan_virtual_time_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        orphan_virtual_time_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(orphan_virtual_time, kernel)["ok"] is False,
        "virtual-time divergence coordinate requires a scheduler-turn coordinate",
    )

    impossible_pending_status = deepcopy(timeout_campaigns["timeout"])
    impossible_pending_row = next(
        row for row in impossible_pending_status if row["test"] == cells[5]["test"]
    )
    impossible_pending_row["attempts"][0]["status"] = 0
    impossible_pending_row["attempts"][0]["signal"] = None
    require(
        jq_validate(impossible_pending_status, kernel)["ok"] is False,
        "pending NotRun timeout cannot claim an impossible status-zero completion",
    )

    malformed_timeout_first_run = deepcopy(timeout_campaigns["timeout_first_run"])
    malformed_timeout_first_run_row = next(
        row
        for row in malformed_timeout_first_run
        if row["test"] == cells[6]["test"]
    )
    malformed_timeout_first_run_report = json.loads(
        malformed_timeout_first_run_row["attempts"][0]["verification_report"]
    )
    malformed_timeout_first_run_report["no_result_reason"]["stdout_bytes"] = 0.5
    malformed_timeout_first_run_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(malformed_timeout_first_run_report)
    malformed_timeout_first_run_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        malformed_timeout_first_run_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(malformed_timeout_first_run, kernel)["ok"] is False,
        "fractional first-run-rejected timeout byte count fails closed",
    )
    impossible_timeout_first_run_status = deepcopy(
        timeout_campaigns["timeout_first_run"]
    )
    impossible_timeout_first_run_row = next(
        row
        for row in impossible_timeout_first_run_status
        if row["test"] == cells[6]["test"]
    )
    impossible_timeout_first_run_row["attempts"][0]["status"] = 7
    require(
        jq_validate(impossible_timeout_first_run_status, kernel)["ok"] is False,
        "post-exit first-run-rejected timeout requires outer status 125",
    )
    wall_first_run_race = deepcopy(timeout_campaigns["timeout_first_run"])
    for row in wall_first_run_race:
        if row["test"] == cells[6]["test"]:
            row["error_kind"] = "wall-timeout"
            row["attempts"][0]["error_kind"] = "wall-timeout"
            row["reason"] = "cell exceeded 57 wall s backstop (22 s CPU budget)"
            row["attempts"][0]["reason"] = row["reason"]
            row["duration_ms"] = 57_000
    require(
        jq_validate(wall_first_run_race, kernel)["ok"] is True,
        "wall-timeout may retain a raced status-125 first-run report",
    )

    mismatched_timeout_divergence = deepcopy(timeout_campaigns["timeout_divergence"])
    mismatched_timeout_divergence_row = next(
        row
        for row in mismatched_timeout_divergence
        if row["test"] == cells[7]["test"]
    )
    mismatched_timeout_divergence_row["attempts"][0]["first_divergent_record"] = 8
    require(
        jq_validate(mismatched_timeout_divergence, kernel)["ok"] is False,
        "timeout divergence coordinates must match row and report",
    )
    impossible_timeout_divergence_status = deepcopy(
        timeout_campaigns["timeout_divergence"]
    )
    impossible_timeout_divergence_row = next(
        row
        for row in impossible_timeout_divergence_status
        if row["test"] == cells[7]["test"]
    )
    impossible_timeout_divergence_row["attempts"][0]["status"] = 7
    require(
        jq_validate(impossible_timeout_divergence_status, kernel)["ok"] is False,
        "post-exit divergent timeout requires outer status 1",
    )
    wall_divergence_race = deepcopy(timeout_campaigns["timeout_divergence"])
    for row in wall_divergence_race:
        if row["test"] == cells[7]["test"]:
            row["error_kind"] = "wall-timeout"
            row["attempts"][0]["error_kind"] = "wall-timeout"
            row["reason"] = "cell exceeded 57 wall s backstop (22 s CPU budget)"
            row["attempts"][0]["reason"] = row["reason"]
            row["duration_ms"] = 57_000
    require(
        jq_validate(wall_divergence_race, kernel)["ok"] is True,
        "wall-timeout may retain a raced status-1 divergence report",
    )

    malformed_signaled_match = deepcopy(timeout_campaigns["timeout_signaled_match"])
    malformed_signaled_match_row = next(
        row
        for row in malformed_signaled_match
        if row["test"] == cells[8]["test"]
    )
    malformed_signaled_match_row["attempts"][0]["status"] = 0
    require(
        jq_validate(malformed_signaled_match, kernel)["ok"] is False,
        "completed timeout cannot claim both exit status and signal",
    )

    malformed_zero_count_timeout = deepcopy(
        timeout_campaigns["timeout_zero_divergence"]
    )
    malformed_zero_count_timeout_row = next(
        row
        for row in malformed_zero_count_timeout
        if row["test"] == cells[10]["test"]
    )
    malformed_zero_count_timeout_row["outcome"] = "FAIL"
    malformed_zero_count_timeout_row["attempts"][0]["outcome"] = "FAIL"
    require(
        jq_validate(malformed_zero_count_timeout, kernel)["ok"] is False,
        "zero-count timeout comparison cannot claim canonical product evidence",
    )

    fractional_signal_timeout = deepcopy(all_timeout)
    fractional_signal_timeout[0]["attempts"][0]["signal"] = 15.5
    require(
        jq_validate(fractional_signal_timeout, kernel)["ok"] is False,
        "fractional timeout signal fails closed",
    )

    impossible_status7_timeout = deepcopy(timeout_campaigns["timeout_signaled_match"])
    impossible_status7_timeout_row = next(
        row
        for row in impossible_status7_timeout
        if row["test"] == cells[8]["test"]
    )
    impossible_status7_timeout_row["attempts"][0]["status"] = 7
    impossible_status7_timeout_row["attempts"][0]["signal"] = None
    require(
        jq_validate(impossible_status7_timeout, kernel)["ok"] is False,
        "matched timeout report rejects an unreachable outer status 7",
    )

    unequal_timeout_match = deepcopy(timeout_campaigns["timeout_signaled_match"])
    unequal_timeout_match_row = next(
        row
        for row in unequal_timeout_match
        if row["test"] == cells[8]["test"]
    )
    unequal_timeout_match_report = json.loads(
        unequal_timeout_match_row["attempts"][0]["verification_report"]
    )
    unequal_timeout_match_report["compared_log_messages"]["right"] = 4
    unequal_timeout_match_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(unequal_timeout_match_report)
    unequal_timeout_match_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        unequal_timeout_match_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(unequal_timeout_match, kernel)["ok"] is False,
        "matched timeout report rejects unequal compared-INFO counts",
    )

    impossible_timeout_guest = deepcopy(timeout_campaigns["timeout_signaled_match"])
    impossible_timeout_guest_row = next(
        row
        for row in impossible_timeout_guest
        if row["test"] == cells[8]["test"]
    )
    impossible_timeout_guest_report = json.loads(
        impossible_timeout_guest_row["attempts"][0]["verification_report"]
    )
    impossible_timeout_guest_report["guest_exit_code"] = 7
    impossible_timeout_guest_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(impossible_timeout_guest_report)
    impossible_timeout_guest_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        impossible_timeout_guest_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(impossible_timeout_guest, kernel)["ok"] is False,
        "timeout-overlaid canonical report requires guest status 0",
    )

    flipped_zero_match_parity = deepcopy(timeout_campaigns["timeout_zero_match"])
    flipped_zero_match_row = next(
        row
        for row in flipped_zero_match_parity
        if row["test"] == cells[13]["test"]
    )
    flipped_zero_match_report = json.loads(
        flipped_zero_match_row["attempts"][0]["verification_report"]
    )
    flipped_zero_match_report["bitwise_parity"] = True
    flipped_zero_match_row["attempts"][0][
        "verification_report"
    ] = serialize_disk_report(flipped_zero_match_report)
    flipped_zero_match_row["attempts"][0][
        "verification_report_sha256"
    ] = digest_bytes(
        flipped_zero_match_row["attempts"][0]["verification_report"].encode()
    )
    require(
        jq_validate(flipped_zero_match_parity, kernel)["ok"] is False,
        "matched zero-count timeout rejects impossible bitwise parity",
    )

    malformed_no_report_timeout = deepcopy(timeout_campaigns["timeout_no_report"])
    malformed_no_report_row = next(
        row
        for row in malformed_no_report_timeout
        if row["test"] == cells[11]["test"]
    )
    malformed_no_report_row["attempts"][0]["verification_report_sha256"] = "0" * 64
    require(
        jq_validate(malformed_no_report_timeout, kernel)["ok"] is False,
        "null timeout report with a fabricated report hash fails closed",
    )

    infrastructure_timeout = deepcopy(all_timeout)
    infrastructure_timeout[0]["error_kind"] = "infrastructure"
    infrastructure_timeout[0]["attempts"][0]["error_kind"] = "infrastructure"
    require(
        jq_validate(infrastructure_timeout, kernel)["ok"] is False,
        "infrastructure error kind cannot masquerade as a typed timeout",
    )

    cpu_overrun_test = cells[3]
    cpu_overrun_campaign = without_invocation(all_timeout, cpu_overrun_test["test"], 1)
    cpu_overrun_campaign.extend(
        [
            make_row(cpu_overrun_test, 1, 1, "cpu_overrun", kernel=kernel),
            make_row(cpu_overrun_test, 1, 2, "cpu_overrun", kernel=kernel),
        ]
    )
    cpu_overrun_summary = jq_validate(cpu_overrun_campaign, kernel)
    require(cpu_overrun_summary["ok"] is True, "post-exit CPU overrun is valid excluded evidence")
    require(cpu_overrun_summary["post_exit_cpu_timeout_rows"] == 2, "post-exit CPU overrun retains both retries")
    low_attempt_cpu = deepcopy(cpu_overrun_campaign)
    low_attempt_cpu_row = next(
        row for row in low_attempt_cpu if row["test"] == cpu_overrun_test["test"]
    )
    low_attempt_cpu_row["attempts"][0]["cpu_usage_usec"] = 21_999_999
    low_attempt_cpu_row["cpu_usage_usec"] = 22_000_000
    require(
        jq_validate(low_attempt_cpu, kernel)["ok"] is False,
        "CPU timeout floor is measured on attempt CPU, not preparation-inclusive cell CPU",
    )
    malformed_cpu_overrun = deepcopy(cpu_overrun_campaign)
    cpu_overrun_row = next(
        row for row in malformed_cpu_overrun if row["test"] == cpu_overrun_test["test"]
    )
    cpu_overrun_row["error_kind"] = "wall-timeout"
    cpu_overrun_row["attempts"][0]["error_kind"] = "wall-timeout"
    require(
        jq_validate(malformed_cpu_overrun, kernel)["ok"] is False,
        "timeout kind cannot change without its producer-owned reason",
    )
    wall_status_zero = deepcopy(cpu_overrun_campaign)
    for row in wall_status_zero:
        if row["test"] == cpu_overrun_test["test"]:
            row["error_kind"] = "wall-timeout"
            row["attempts"][0]["error_kind"] = "wall-timeout"
            row["reason"] = "cell exceeded 57 wall s backstop (22 s CPU budget)"
            row["attempts"][0]["reason"] = row["reason"]
            row["duration_ms"] = 57_000
    require(
        jq_validate(wall_status_zero, kernel)["ok"] is True,
        "wall-timeout may retain a naturally raced status-zero completion",
    )
    short_wall_timeout = deepcopy(all_timeout)
    short_wall_timeout[0]["duration_ms"] = 56_999
    require(
        jq_validate(short_wall_timeout, kernel)["ok"] is False,
        "wall-timeout row cannot predate its exact 57-second deadline",
    )

    pre_timeout_test = cells[4]
    pre_timeout_campaign = without_invocation(all_timeout, pre_timeout_test["test"], 1)
    pre_timeout_campaign.extend(
        [
            make_row(pre_timeout_test, 1, 1, "pre_timeout", kernel=kernel),
            make_row(pre_timeout_test, 1, 2, "pre_timeout", kernel=kernel),
        ]
    )
    pre_timeout_summary = jq_validate(pre_timeout_campaign, kernel)
    require(pre_timeout_summary["ok"] is True, "pre-attempt timeout is valid excluded evidence")
    require(pre_timeout_summary["pre_attempt_timeout_rows"] == 2, "pre-attempt timeout retains both retries")
    malformed_pre_timeout = deepcopy(pre_timeout_campaign)
    pre_timeout_row = next(
        row for row in malformed_pre_timeout if row["test"] == pre_timeout_test["test"]
    )
    pre_timeout_row["attempts"][0]["stderr"] = "process unexpectedly started"
    require(
        jq_validate(malformed_pre_timeout, kernel)["ok"] is False,
        "pre-attempt timeout with executed-process evidence fails closed",
    )
    invented_pre_timeout_cpu = deepcopy(pre_timeout_campaign)
    invented_pre_timeout_cpu_row = next(
        row
        for row in invented_pre_timeout_cpu
        if row["test"] == pre_timeout_test["test"]
    )
    invented_pre_timeout_cpu_row["cpu_usage_usec"] = 22_000_000
    require(
        jq_validate(invented_pre_timeout_cpu, kernel)["ok"] is False,
        "pre-attempt timeout rejects invented cell CPU accounting",
    )
    impossible_pre_timeout_kind = deepcopy(pre_timeout_campaign)
    impossible_pre_timeout_kind_row = next(
        row
        for row in impossible_pre_timeout_kind
        if row["test"] == pre_timeout_test["test"]
    )
    impossible_pre_timeout_kind_row["error_kind"] = "cpu-timeout"
    impossible_pre_timeout_kind_row["attempts"][0]["error_kind"] = "cpu-timeout"
    impossible_pre_timeout_kind_row["reason"] = (
        "cell exceeded 22 CPU s before attempt 1 started"
    )
    impossible_pre_timeout_kind_row["attempts"][0]["reason"] = (
        impossible_pre_timeout_kind_row["reason"]
    )
    require(
        jq_validate(impossible_pre_timeout_kind, kernel)["ok"] is False,
        "fresh verify attempt cannot exhaust its CPU budget before launch",
    )

    adaptive = without_invocation(all_timeout, target["test"], 1)
    adaptive.append(make_row(target, 1, 1, "pass", kernel=kernel))
    adaptive.extend(
        [
            make_row(target, 2, 1, "divergence", kernel=kernel),
            make_row(target, 2, 2, "divergence", kernel=kernel),
            make_row(target, 3, 1, "pass", kernel=kernel),
        ]
    )
    adaptive_summary = jq_validate(adaptive, kernel)
    require(adaptive_summary["ok"] is True, "round-1 strict PASS schedules both rounds 2 and 3")
    require(adaptive_summary["expected_invocation_count"] == 121, "adaptive invocation count is 119 + 2")
    require(adaptive_summary["provisional_qualified_cell_count"] == 0, "round-2 failure excludes promotion despite round 3")

    qualified = without_invocation(all_timeout, target["test"], 1)
    qualified.append(make_row(target, 1, 1, "pass", kernel=kernel))
    qualified.extend(
        [make_row(target, 2, 1, "pass", kernel=kernel), make_row(target, 3, 1, "pass", kernel=kernel)]
    )
    qualified_summary = jq_validate(qualified, kernel)
    require(qualified_summary["ok"] is True, "three strict no-retry passes are evidence-valid")
    require(qualified_summary["provisional_qualified_cell_count"] == 1, "three strict no-retry passes qualify exactly one cell")

    retry_campaign = without_invocation(all_timeout, target["test"], 1) + retry_rows
    retry_summary = jq_validate(retry_campaign, kernel)
    require(retry_summary["ok"] is True, "retry rows remain valid preserved evidence")
    require(retry_summary["round1_eligible_count"] == 0, "retry success remains excluded")

    lone_timeout = without_invocation(all_timeout, target["test"], 1)
    lone_timeout.append(make_row(target, 1, 1, "timeout", kernel=kernel))
    require(
        jq_validate(lone_timeout, kernel)["ok"] is False,
        "lone attempt-1 timeout is incomplete and fails closed",
    )

    require(jq_validate(adaptive[:-1], kernel)["ok"] is False, "missing adaptive row fails closed")
    require(jq_validate(all_timeout + [all_timeout[0]], kernel)["ok"] is False, "duplicate row fails closed")
    unexpected = list(all_timeout) + [
        make_row(cells[2], 2, 1, "timeout", kernel=kernel),
        make_row(cells[2], 2, 2, "timeout", kernel=kernel),
    ]
    require(jq_validate(unexpected, kernel)["ok"] is False, "unexpected continuation fails closed")
    host_inapplicable = without_invocation(all_timeout, target["test"], 1)
    for attempt in (1, 2):
        host_row = deepcopy(make_row(target, 1, attempt, "timeout", kernel=kernel))
        host_row["outcome"] = "SKIP"
        host_row["result"] = "host-inapplicable"
        host_row["error_kind"] = "host-inapplicable"
        host_row["attempts"][0]["outcome"] = "SKIP"
        host_row["attempts"][0]["error_kind"] = "host-inapplicable"
        host_inapplicable.append(host_row)
    require(jq_validate(host_inapplicable, kernel)["ok"] is False, "HOST-INAPPLICABLE fails closed")

    if rows_only:
        print("PASS row-only suite intentionally deferred artifact/freeze checks")
        print("PASS no build/Hermit/KVM/systemd action was executed")
        return 0

    launcher = load_launcher()
    trusted_launcher = load_launcher(trusted=True)
    require(
        launcher.CAMPAIGN == SCRIPT_DIR
        and launcher.CHECKOUT == ROOT
        and launcher.AGENT == "kvm-ratchet-90-run6"
        and str(launcher.EXPECTED_RUN_CHECKOUT).endswith("/slots/kvm-ratchet-90-run6"),
        "launcher derives the campaign/checkout from its own location and attributes the run owner",
    )
    require(
        "/home/newton/work/dev-hermit/worktrees/slots/kvm-ratchet-90" not in DRIVER.read_text(),
        "payload contains no staging-slot checkout path",
    )
    direct_launch_rejected = False
    try:
        launcher.launch(dry_run=True)
    except RuntimeError as error:
        direct_launch_rejected = "dual-digest" in str(error)
    require(
        direct_launch_rejected,
        "state-changing launch refuses an untrusted direct pathname invocation",
    )
    launch_sha = digest_file(LAUNCHER)
    freeze_sha = digest_file(FREEZE_MANIFEST)
    trusted_command = launcher.trusted_entry_command(
        launch_sha, freeze_sha, "launch"
    )
    require(
        trusted_command[:4] == ["/usr/bin/python3", "-I", "-S", "-c"]
        and trusted_command[4] == launcher.INITIAL_BOOTSTRAP
        and trusted_command[5:10]
        == [
            launch_sha,
            freeze_sha,
            str(launcher.EXPECTED_RUN_CAMPAIGN / "launch.py"),
            str(launcher.EXPECTED_RUN_CAMPAIGN / "FROZEN_SHA256SUMS"),
            str(launcher.EXPECTED_RUN_CAMPAIGN),
        ]
        and trusted_command[10:] == ["launch"],
        "external bootstrap carries independent literal launcher and freeze-manifest digests",
    )
    completed_fields = launcher.terminal_fields(0)
    failed_fields = launcher.terminal_fields(9, "fixture failure")
    require(
        completed_fields["state"] == "completed"
        and completed_fields["result"] == "passed"
        and completed_fields["exit_code"] == 0,
        "completed bench terminal record passes the registry schema",
    )
    require(
        failed_fields["state"] == "failed"
        and failed_fields["result"] == "failed"
        and failed_fields["exit_code"] == 9,
        "failed bench terminal record passes the registry schema",
    )
    invalid_terminal_rejected = False
    try:
        launcher.run_registry.parse_current_record(
            launcher.initial_record()
            | {
                "state": "completed",
                "result": "evidence-complete",
                "exit_code": 0,
                "finished_at": launcher.utc_now(),
            }
        )
    except (RuntimeError, ValueError):
        invalid_terminal_rejected = True
    require(invalid_terminal_rejected, "non-schema bench terminal result is rejected")
    driver_sha = digest_file(DRIVER)
    payload_self_check = subprocess.run(
        ["bash", str(DRIVER), "--self-test-payload-hash"],
        env=os.environ | {"KVM_RATCHET_PAYLOAD_SHA256": driver_sha},
        cwd="/tmp",
        text=True,
        capture_output=True,
        check=False,
    )
    require(
        payload_self_check.returncode == 0,
        "payload immediately accepts its launcher-bound digest without side effects",
    )
    payload_self_tamper = subprocess.run(
        ["bash", str(DRIVER), "--self-test-payload-hash"],
        env=os.environ | {"KVM_RATCHET_PAYLOAD_SHA256": "0" * 64},
        text=True,
        capture_output=True,
        check=False,
    )
    require(
        payload_self_tamper.returncode != 0,
        "payload immediately rejects a launcher-bound digest mismatch",
    )
    with tempfile.TemporaryDirectory(prefix="kvm-ratchet-payload-guard-") as guard_tmp:
        guarded_payload = Path(guard_tmp) / "payload.sh"
        marker = Path(guard_tmp) / "ran"
        guarded_payload.write_text(f"#!/bin/bash\nprintf ran > {marker}\n")
        reviewed_hash = digest_file(guarded_payload)
        guard_ok = subprocess.run(
            launcher.guarded_payload_command(
                guarded_payload, reviewed_hash, "f" * 64
            ),
            text=True,
            capture_output=True,
            check=False,
        )
        require(
            guard_ok.returncode == 0 and marker.read_text() == "ran",
            "post-queue guard executes only the reviewed payload bytes",
        )
        marker.unlink()
        guarded_payload.write_text("#!/bin/bash\nprintf tampered > /dev/null\n")
        guard_tamper = subprocess.run(
            launcher.guarded_payload_command(
                guarded_payload, reviewed_hash, "f" * 64
            ),
            text=True,
            capture_output=True,
            check=False,
        )
        require(
            guard_tamper.returncode == 75 and not marker.exists(),
            "post-queue guard rejects payload mutation before Bash opens it",
        )
    with tempfile.TemporaryDirectory(
        prefix="kvm-ratchet-worker-snapshot-race-"
    ) as snapshot_tmp:
        snapshot_root = Path(snapshot_tmp)
        marker = snapshot_root / "executed"
        source_paths = {
            name: snapshot_root / name for name in launcher.RUNTIME_INPUT_NAMES
        }
        reviewed_expected = b"reviewed expected cells\n"
        reviewed_payload = (
            "#!/bin/bash\nset -eu\n"
            f"printf 'reviewed:' > {shlex.quote(str(marker))}\n"
            f"/bin/cat \"$KVM_RATCHET_EXPECTED_CELLS_PATH\" >> {shlex.quote(str(marker))}\n"
        ).encode()
        freeze_bytes = b"fixture frozen manifest bytes\n"
        input_bytes = {
            name: (
                reviewed_payload
                if name == DRIVER.name
                else reviewed_expected
                if name == EXPECTED.name
                else freeze_bytes
                if name == FREEZE_MANIFEST.name
                else f"reviewed {name}\n".encode()
            )
            for name in launcher.RUNTIME_INPUT_NAMES
        }
        for name, path in source_paths.items():
            path.write_bytes(input_bytes[name])
        expected_hashes = {
            name: digest_bytes(content)
            for name, content in input_bytes.items()
            if name != FREEZE_MANIFEST.name
        }
        freeze_hash = digest_bytes(freeze_bytes)
        captured = launcher.capture_runtime_input_bytes(
            source_paths,
            expected_hashes,
            freeze_manifest_source=freeze_bytes,
            freeze_manifest_sha256=freeze_hash,
        )
        capsule = launcher.encode_runtime_input_capsule(captured)
        require(
            launcher.decode_runtime_input_capsule(
                capsule, freeze_hash, expected_hashes
            )
            == captured,
            "worker capsule round-trips the exact reviewed runtime input bytes",
        )
        snapshot = launcher.create_runtime_input_snapshots(
            captured, freeze_hash, expected_hashes
        )
        try:
            require(
                all(fd >= 100 and fd != 9 for fd in snapshot["fds"]),
                "sealed runtime inputs use only the reserved high-fd range",
            )
            require(
                all(
                    fcntl.fcntl(fd, fcntl.F_GET_SEALS)
                    & (
                        fcntl.F_SEAL_WRITE
                        | fcntl.F_SEAL_GROW
                        | fcntl.F_SEAL_SHRINK
                        | fcntl.F_SEAL_SEAL
                    )
                    == (
                        fcntl.F_SEAL_WRITE
                        | fcntl.F_SEAL_GROW
                        | fcntl.F_SEAL_SHRINK
                        | fcntl.F_SEAL_SEAL
                    )
                    for fd in snapshot["fds"]
                ),
                "every runtime input memfd is write/grow/shrink/seal sealed",
            )
            for path in source_paths.values():
                path.write_bytes(b"replacement pathname bytes\n")
            guarded = launcher.guarded_payload_command(
                snapshot["paths"][DRIVER.name],
                expected_hashes[DRIVER.name],
                freeze_hash,
                snapshot_paths=snapshot["paths"],
            )
            gate = [
                "/bin/sh",
                "-c",
                'exec 9</dev/null; exec "$@"',
                "validate-lock-fd9-gate",
                *guarded,
            ]
            immutable_run = subprocess.run(
                gate,
                pass_fds=snapshot["fds"],
                text=True,
                capture_output=True,
                check=False,
            )
            require(
                immutable_run.returncode == 0
                and marker.read_bytes() == b"reviewed:" + reviewed_expected,
                "fd9 gate executes the sealed payload and reads the sealed subordinate after pathname replacement",
            )
        finally:
            launcher.close_runtime_input_snapshots(snapshot)
    with tempfile.TemporaryDirectory(
        prefix="kvm-ratchet-post-boundary-snapshot-"
    ) as boundary_tmp:
        boundary_root = Path(boundary_tmp)
        boundary_marker = boundary_root / "payload-executed"
        bash_env_marker = boundary_root / "bash-env-executed"
        bash_env_source = boundary_root / "hostile-bash-env"
        bash_env_source.write_text(
            f"printf sourced > {shlex.quote(str(bash_env_marker))}\n"
        )
        sitecustomize_marker = boundary_root / "sitecustomize-executed"
        hostile_pythonpath = boundary_root / "hostile-pythonpath"
        hostile_pythonpath.mkdir()
        (hostile_pythonpath / "sitecustomize.py").write_text(
            "from pathlib import Path\n"
            f"Path({str(sitecustomize_marker)!r}).write_text('sourced')\n"
        )
        function_marker = boundary_root / "bash-function-executed"
        hostile_git_dir = boundary_root / "hostile-git-dir"
        boundary_freeze = b"post-boundary fixture freeze manifest\n"
        boundary_inputs = {
            name: (
                boundary_freeze
                if name == FREEZE_MANIFEST.name
                else f"post-boundary fixture {name}\n".encode()
            )
            for name in launcher.RUNTIME_INPUT_NAMES
            if name != DRIVER.name
        }
        pre_boundary_fds = []
        pre_boundary_paths = {}
        for name in launcher.RUNTIME_INPUT_NAMES:
            sentinel = f"pre-boundary sealed authority {name}\n".encode()
            descriptor, descriptor_path = launcher.create_sealed_input(
                "pre-boundary-" + name,
                sentinel,
                digest_bytes(sentinel),
            )
            pre_boundary_fds.append(descriptor)
            pre_boundary_paths[name] = descriptor_path
        pre_boundary_snapshot = {
            "fds": tuple(pre_boundary_fds),
            "paths": pre_boundary_paths,
        }
        old_fd_identities = {
            str(descriptor): [
                os.fstat(descriptor).st_dev,
                os.fstat(descriptor).st_ino,
            ]
            for descriptor in pre_boundary_snapshot["fds"]
        }
        embedded_nonpayload_hashes = {
            name: digest_bytes(content)
            for name, content in boundary_inputs.items()
        }
        probe_payload_source = r'''#!/bin/bash
set -euo pipefail
if [[ -v BASH_ENV || -v ENV || -v PYTHONPATH || -v GIT_DIR ]] || compgen -A function | grep -q .; then
  printf 'shell startup injection survived post-boundary sanitization\n' >&2
  exit 92
fi
/usr/local/bin/git -C __CHECKOUT__ rev-parse HEAD | /usr/bin/grep -qx __SOURCE_SHA__
exec /usr/bin/python3 - <<'PY'
import fcntl
import hashlib
import os

names=("FROZEN_SHA256SUMS","run-kvm-ratchet-90.sh","expected-cells.json","validate-population.jq","validate-results.jq","validate-evidence.sh","validate-strict-invocation-artifacts.sh")
path_environment={"FROZEN_SHA256SUMS":"KVM_RATCHET_FREEZE_MANIFEST_PATH","run-kvm-ratchet-90.sh":"KVM_RATCHET_PAYLOAD_PATH","expected-cells.json":"KVM_RATCHET_EXPECTED_CELLS_PATH","validate-population.jq":"KVM_RATCHET_POPULATION_VALIDATOR_PATH","validate-results.jq":"KVM_RATCHET_RESULTS_VALIDATOR_PATH","validate-evidence.sh":"KVM_RATCHET_EVIDENCE_VALIDATOR_PATH","validate-strict-invocation-artifacts.sh":"KVM_RATCHET_STRICT_ARTIFACT_VALIDATOR_PATH"}
expected=__EXPECTED_HASHES__
expected["run-kvm-ratchet-90.sh"]=os.environ["KVM_RATCHET_PAYLOAD_SHA256"]
old_identities=__OLD_IDENTITIES__
if set(expected) != set(names):
    raise SystemExit("fixture digest set changed")
branch_parts=os.environ["CI_HUB_VALIDATE_BRANCH"].split(":")
if len(branch_parts) != 3 or branch_parts[0] != "fixture-branch" or os.getpid() != int(branch_parts[1]):
    raise SystemExit("PID changed after the start-gate exec")
if os.getpgrp() != int(branch_parts[2]):
    raise SystemExit("process group changed after the start-gate exec")
if [os.environ.get(name) for name in ("CI_HUB_PAYLOAD_DOMAIN","CI_HUB_VALIDATE_LOCK_OWNER_PID","CI_HUB_VALIDATE_LOCK_OWNER_FILE","CI_HUB_VALIDATE_RUN_NUMBER")] != ["fixture-domain","4242","/tmp/fixture-owner","7"]:
    raise SystemExit("validate-lock attribution environment was not preserved")
required=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL
observed_fds=[]
for name in names:
    path=os.environ[path_environment[name]]
    if not path.startswith("/proc/self/fd/"):
        raise SystemExit("runtime input did not use a descriptor path: "+name)
    descriptor=int(path.rsplit("/",1)[1])
    if descriptor < 100 or descriptor == 9:
        raise SystemExit("runtime input descriptor is outside the reserved range: "+name)
    if fcntl.fcntl(descriptor,fcntl.F_GETFD)&fcntl.FD_CLOEXEC:
        raise SystemExit("runtime input descriptor retained CLOEXEC: "+name)
    if not os.get_inheritable(descriptor):
        raise SystemExit("runtime input descriptor is not inheritable: "+name)
    if fcntl.fcntl(descriptor,fcntl.F_GET_SEALS)&required != required:
        raise SystemExit("runtime input descriptor is not fully sealed: "+name)
    metadata=os.fstat(descriptor)
    chunks=[]
    offset=0
    while offset < metadata.st_size:
        chunk=os.pread(descriptor,min(1048576,metadata.st_size-offset),offset)
        if not chunk:
            raise SystemExit("short read from runtime input: "+name)
        chunks.append(chunk)
        offset+=len(chunk)
    if hashlib.sha256(b"".join(chunks)).hexdigest() != expected[name]:
        raise SystemExit("runtime input digest changed after exec: "+name)
    observed_fds.append(descriptor)
if len(set(observed_fds)) != 7:
    raise SystemExit("post-boundary runtime input descriptors are not unique")
for descriptor_text,identity in old_identities.items():
    try:
        metadata=os.fstat(int(descriptor_text))
    except OSError:
        continue
    if [metadata.st_dev,metadata.st_ino] == identity:
        raise SystemExit("pre-boundary descriptor survived tool-cost close_fds")
with open(__MARKER__,"xb") as marker:
    marker.write(b"post-boundary-seven-ok\n")
PY
'''
        probe_payload = (
            probe_payload_source.replace(
                "__CHECKOUT__", shlex.quote(str(ROOT))
            )
            .replace("__SOURCE_SHA__", shlex.quote(SOURCE_SHA))
            .replace("__EXPECTED_HASHES__", repr(embedded_nonpayload_hashes))
            .replace("__OLD_IDENTITIES__", repr(old_fd_identities))
            .replace("__MARKER__", repr(str(boundary_marker)))
            .encode()
        )
        boundary_inputs[DRIVER.name] = probe_payload
        boundary_hashes = {
            name: digest_bytes(content)
            for name, content in boundary_inputs.items()
            if name != FREEZE_MANIFEST.name
        }
        boundary_freeze_hash = digest_bytes(boundary_freeze)
        boundary_capsule = launcher.encode_runtime_input_capsule(boundary_inputs)
        boundary_environment = os.environ | {
            "BASH_ENV": str(bash_env_source),
            "PYTHONPATH": str(hostile_pythonpath),
            "GIT_DIR": str(hostile_git_dir),
            "BASH_FUNC_kvm_ratchet_injected%%": (
                f"() {{ printf injected > {function_marker}; }}"
            ),
        }
        gate_script = r'''set -eu
exec 9</dev/null
if IFS= read -r ignored <&9; then :; fi
exec 9<&-
bash_env=$1
python_path=$2
git_dir=$3
function_definition=$4
shift 4
CI_HUB_PAYLOAD_DOMAIN=fixture-domain
CI_HUB_VALIDATE_LOCK_OWNER_PID=4242
CI_HUB_VALIDATE_LOCK_OWNER_FILE=/tmp/fixture-owner
CI_HUB_VALIDATE_RUN_NUMBER=7
CI_HUB_VALIDATE_BRANCH="fixture-branch:$$:$(/usr/bin/ps -o pgid= -p "$$" | /usr/bin/tr -d ' ')"
BASH_ENV=$bash_env
ENV=$bash_env
PYTHONPATH=$python_path
GIT_DIR=$git_dir
export CI_HUB_PAYLOAD_DOMAIN CI_HUB_VALIDATE_LOCK_OWNER_PID CI_HUB_VALIDATE_LOCK_OWNER_FILE CI_HUB_VALIDATE_RUN_NUMBER CI_HUB_VALIDATE_BRANCH BASH_ENV ENV PYTHONPATH GIT_DIR
exec /usr/bin/env "$function_definition" "$@"
'''

        def encode_boundary_document(document: dict[str, str]) -> str:
            serialized = json.dumps(
                document,
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
            return base64.b64encode(zlib.compress(serialized, level=9)).decode(
                "ascii"
            )

        def boundary_command(post_boundary: list[str]) -> list[str]:
            clean_environment = launcher.worker_environment()
            return [
                "/usr/bin/env",
                "-i",
                *(
                    f"{name}={value}"
                    for name, value in clean_environment.items()
                ),
                "/usr/bin/with-proxy",
                str(launcher.TOOL_ROOT / "ci-hub/bin/tool-cost"),
                "--tool",
                "kvm-ratchet-post-boundary-fixture",
                "--estimate-unknown",
                "--basis",
                "not measured: synthetic close-fds boundary",
                "--",
                "/bin/sh",
                "-c",
                gate_script,
                "validate-lock-fd9-gate",
                str(bash_env_source),
                str(hostile_pythonpath),
                str(hostile_git_dir),
                (
                    "BASH_FUNC_kvm_ratchet_injected%%=() { printf injected > "
                    + shlex.quote(str(function_marker))
                    + "; }"
                ),
                *post_boundary,
            ]

        def run_boundary(post_boundary: list[str]) -> subprocess.CompletedProcess[str]:
            boundary_marker.unlink(missing_ok=True)
            bash_env_marker.unlink(missing_ok=True)
            sitecustomize_marker.unlink(missing_ok=True)
            function_marker.unlink(missing_ok=True)
            return subprocess.run(
                boundary_command(post_boundary),
                cwd=ROOT,
                env=boundary_environment,
                pass_fds=pre_boundary_snapshot["fds"],
                text=True,
                capture_output=True,
                check=False,
            )

        try:
            post_boundary = launcher.post_boundary_payload_command(
                runtime_capsule=boundary_capsule,
                runtime_capsule_digest=launcher.runtime_capsule_sha256(
                    boundary_capsule
                ),
                payload_sha256=boundary_hashes[DRIVER.name],
                freeze_manifest_sha256=boundary_freeze_hash,
                expected_hashes=boundary_hashes,
            )
            require(
                post_boundary[:5]
                == [
                    "/usr/bin/python3",
                    "-I",
                    "-S",
                    "-c",
                    launcher.POST_BOUNDARY_BOOTSTRAP,
                ]
                and post_boundary.count(boundary_capsule) == 1
                and not any(
                    re.fullmatch(r"/proc/self/fd/[0-9]+", argument)
                    for argument in post_boundary[5:]
                )
                and max(len(os.fsencode(argument)) for argument in post_boundary)
                <= 32 * os.sysconf("SC_PAGE_SIZE") - 1,
                "post-boundary command carries one bounded capsule and no pre-boundary fd pathname",
            )
            wrong_transport_digest_rejected = False
            try:
                launcher.post_boundary_payload_command(
                    runtime_capsule=boundary_capsule,
                    runtime_capsule_digest="0" * 64,
                    payload_sha256=boundary_hashes[DRIVER.name],
                    freeze_manifest_sha256=boundary_freeze_hash,
                    expected_hashes=boundary_hashes,
                )
            except RuntimeError:
                wrong_transport_digest_rejected = True
            require(
                wrong_transport_digest_rejected,
                "post-boundary command refuses a capsule that disagrees with its pre-boundary digest",
            )
            boundary_ok = run_boundary(post_boundary)
            require(
                boundary_ok.returncode == 0
                and boundary_marker.read_bytes() == b"post-boundary-seven-ok\n"
                and not bash_env_marker.exists()
                and not sitecustomize_marker.exists()
                and not function_marker.exists()
                and launcher.runtime_capsule_sha256(boundary_capsule)
                in boundary_ok.stdout,
                "real tool-cost close_fds plus fd9 gate recreates all seven readable sealed inheritable inputs",
            )

            tampered_capsule_command = list(post_boundary)
            tampered_capsule_command[5] = (
                ("A" if boundary_capsule[0] != "A" else "B")
                + boundary_capsule[1:]
            )
            tampered_capsule = run_boundary(tampered_capsule_command)
            require(
                tampered_capsule.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects capsule transport tamper before payload execution",
            )

            truncated_capsule_command = list(post_boundary)
            truncated_capsule_command[5] = boundary_capsule[:-4]
            truncated_capsule_command[6] = launcher.runtime_capsule_sha256(
                truncated_capsule_command[5]
            )
            truncated_capsule = run_boundary(truncated_capsule_command)
            require(
                truncated_capsule.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects authenticated truncated zlib framing before payload execution",
            )

            encoded_document = {
                name: base64.b64encode(content).decode("ascii")
                for name, content in boundary_inputs.items()
            }
            missing_document = dict(encoded_document)
            missing_document.pop("validate-results.jq")
            missing_capsule_command = list(post_boundary)
            missing_capsule_command[5] = encode_boundary_document(missing_document)
            missing_capsule_command[6] = launcher.runtime_capsule_sha256(
                missing_capsule_command[5]
            )
            missing_member = run_boundary(missing_capsule_command)
            require(
                missing_member.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects an authenticated capsule with a missing member",
            )

            duplicate_pairs = [
                json.dumps(name, separators=(",", ":"))
                + ":"
                + json.dumps(encoded_document[name], separators=(",", ":"))
                for name in launcher.RUNTIME_INPUT_NAMES
            ]
            duplicate_serialized = (
                "{" + ",".join(duplicate_pairs + [duplicate_pairs[0]]) + "}"
            ).encode()
            duplicate_capsule_command = list(post_boundary)
            duplicate_capsule_command[5] = base64.b64encode(
                zlib.compress(duplicate_serialized, level=9)
            ).decode("ascii")
            duplicate_capsule_command[6] = launcher.runtime_capsule_sha256(
                duplicate_capsule_command[5]
            )
            duplicate_member = run_boundary(duplicate_capsule_command)
            require(
                duplicate_member.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects authenticated duplicate JSON members",
            )

            trailing_serialized = json.dumps(
                encoded_document,
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
            trailing_capsule_command = list(post_boundary)
            trailing_capsule_command[5] = base64.b64encode(
                zlib.compress(trailing_serialized, level=9) + b"trailing"
            ).decode("ascii")
            trailing_capsule_command[6] = launcher.runtime_capsule_sha256(
                trailing_capsule_command[5]
            )
            trailing_frame = run_boundary(trailing_capsule_command)
            require(
                trailing_frame.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects authenticated trailing zlib data",
            )

            oversized_document = dict(encoded_document)
            oversized_document["expected-cells.json"] = base64.b64encode(
                b"x" * (launcher.MAX_RUNTIME_INPUT_BYTES + 1)
            ).decode("ascii")
            oversized_member_command = list(post_boundary)
            oversized_member_command[5] = encode_boundary_document(
                oversized_document
            )
            oversized_member_command[6] = launcher.runtime_capsule_sha256(
                oversized_member_command[5]
            )
            oversized_member = run_boundary(oversized_member_command)
            require(
                oversized_member.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects an authenticated oversized member",
            )

            member_tamper_document = dict(encoded_document)
            member_tamper_document["expected-cells.json"] = base64.b64encode(
                b"tampered expected cells\n"
            ).decode("ascii")
            member_tamper_command = list(post_boundary)
            member_tamper_command[5] = encode_boundary_document(
                member_tamper_document
            )
            member_tamper_command[6] = launcher.runtime_capsule_sha256(
                member_tamper_command[5]
            )
            member_tamper = run_boundary(member_tamper_command)
            require(
                member_tamper.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects authenticated member-byte tamper before payload execution",
            )

            cloexec_bootstrap = launcher.POST_BOUNDARY_BOOTSTRAP.replace(
                "os.set_inheritable(high_fd,True)",
                "os.set_inheritable(high_fd,False)",
                1,
            )
            require(
                cloexec_bootstrap != launcher.POST_BOUNDARY_BOOTSTRAP
                and cloexec_bootstrap.count("os.set_inheritable(high_fd,False)") == 1,
                "CLOEXEC mutation changes exactly the post-boundary inheritable transition",
            )
            cloexec_command = list(post_boundary)
            cloexec_command[4] = cloexec_bootstrap
            cloexec_failure = run_boundary(cloexec_command)
            require(
                cloexec_failure.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap refuses a recreated descriptor that would be lost on exec",
            )

            missing_argument = run_boundary(post_boundary[:-1])
            require(
                missing_argument.returncode != 0 and not boundary_marker.exists(),
                "post-boundary bootstrap rejects an incomplete argv contract",
            )

            with mock.patch.object(launcher, "FROZEN_HASHES", boundary_hashes):
                constructed_worker = launcher.worker_command(
                    runtime_capsule=boundary_capsule,
                    runtime_capsule_digest=launcher.runtime_capsule_sha256(
                        boundary_capsule
                    ),
                    payload_sha256=boundary_hashes[DRIVER.name],
                    freeze_manifest_sha256=boundary_freeze_hash,
                )
            worker_delimiter = constructed_worker.index("--")
            worker_measurement = launcher.exec_vector_measurement(
                constructed_worker,
                dict(os.environ),
            )
            require(
                constructed_worker[:worker_delimiter]
                == [
                    "/home/newton/work/dev-hermit/ci-hub/ci-hub",
                    "validate-lock",
                    "run",
                    "--agent",
                    "kvm-ratchet-90-run6",
                    "--kind",
                    "bench",
                    "--target",
                    SOURCE_SHA,
                    "--run-record",
                    str(launcher.RECORD),
                    "--wait",
                    "1800",
                    "--hold",
                    "600",
                    "--child-deadline",
                    "7200",
                ]
                and constructed_worker[worker_delimiter + 1 :] == post_boundary
                and worker_measurement["max_argument_bytes"]
                <= worker_measurement["max_argument_limit"]
                and worker_measurement["total_bytes"]
                < worker_measurement["arg_max"],
                "validate-lock attribution/deadline argv is unchanged and its post-boundary vector fits exec limits",
            )
        finally:
            launcher.close_runtime_input_snapshots(pre_boundary_snapshot)
    with tempfile.TemporaryDirectory(
        prefix="kvm-ratchet-pre-boundary-close-"
    ) as close_tmp:
        close_root = Path(close_tmp)
        close_record = close_root / "current.json"
        close_log = close_root / "service.log"
        admission_fd = os.memfd_create(
            "kvm-ratchet-pre-boundary-close",
            os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING,
        )
        os.set_inheritable(admission_fd, True)
        close_capture: dict[str, object] = {}

        def observe_closed_worker(command, **kwargs):
            close_capture["command"] = command
            close_capture["kwargs"] = kwargs
            try:
                os.fstat(admission_fd)
            except OSError:
                close_capture["admission_fd_closed"] = True
            else:
                close_capture["admission_fd_closed"] = False
            return subprocess.CompletedProcess(command, 17)

        close_receipt = {
            "source_sha": SOURCE_SHA,
            "source_tree": SOURCE_TREE,
            "payload_sha256": "0" * 64,
            "freeze_manifest_sha256": "f" * 64,
        }
        with (
            mock.patch.object(trusted_launcher, "RECORD", close_record),
            mock.patch.object(trusted_launcher, "LOG", close_log),
            mock.patch.object(trusted_launcher, "EXPECTED_RUN_CHECKOUT", ROOT),
            mock.patch.object(trusted_launcher, "EXPECTED_RUN_CAMPAIGN", SCRIPT_DIR),
            mock.patch.object(
                trusted_launcher,
                "prepare_worker_snapshot",
                return_value=(
                    close_receipt,
                    {"paths": {}, "fds": (admission_fd,)},
                ),
            ),
            mock.patch.object(
                trusted_launcher,
                "worker_command",
                return_value=["synthetic-validate-lock"],
            ),
            mock.patch.object(
                trusted_launcher.subprocess,
                "run",
                side_effect=observe_closed_worker,
            ),
        ):
            trusted_launcher.run_registry.create_current_record(
                close_record,
                trusted_launcher.initial_record(),
            )
            close_rc = trusted_launcher.run_worker(
                close_record,
                launcher_sha256=trusted_launcher.EXECUTED_LAUNCHER_SHA256,
                freeze_manifest_sha256=trusted_launcher.EXECUTED_FREEZE_MANIFEST_SHA256,
                runtime_capsule="synthetic",
                runtime_capsule_digest=trusted_launcher.runtime_capsule_sha256(
                    "synthetic"
                ),
            )
            close_terminal = trusted_launcher.run_registry.read_record(close_record)
        require(
            close_rc == 17
            and close_capture.get("admission_fd_closed") is True
            and "pass_fds" not in close_capture["kwargs"]
            and close_terminal["state"] == "failed"
            and close_terminal["exit_code"] == 17,
            "worker closes admission-only memfds before validate-lock and preserves its failure code",
        )
    constructed_systemd = launcher.systemd_command(
        launcher_sha256=launch_sha,
        freeze_manifest_sha256=freeze_sha,
        freeze_manifest_source=FREEZE_MANIFEST.read_bytes(),
        launcher_source=LAUNCHER.read_bytes(),
        runtime_capsule="fixture-runtime-capsule",
    )
    systemd_delimiter = constructed_systemd.index("--")
    exact_worker_environment = launcher.worker_environment()
    clean_service_prefix = [
        "/usr/bin/env",
        "-i",
        *(
            f"{name}={value}"
            for name, value in exact_worker_environment.items()
        ),
        "/usr/bin/with-proxy",
        "/usr/bin/python3",
        "-I",
        "-S",
        "-c",
    ]
    service_command = constructed_systemd[systemd_delimiter + 1 :]
    systemd_capsule_index = service_command.index("--runtime-capsule")
    systemd_capsule_digest_index = service_command.index(
        "--runtime-capsule-sha256"
    )
    require(
        "--expand-environment=no" in constructed_systemd[:systemd_delimiter]
        and "--setenv=RUSTUP_TOOLCHAIN=nightly"
        in constructed_systemd[:systemd_delimiter]
        and "--property=LimitFSIZE=1073742000"
        in constructed_systemd[:systemd_delimiter]
        and exact_worker_environment
        == {
            "HOME": "/home/newton",
            "PATH": "/home/newton/.cargo/bin:/usr/local/bin:/usr/bin:/bin",
            "PYTHONUNBUFFERED": "1",
            "DEV_HERMIT_PARENT": "/home/newton/work/dev-hermit",
            "DEV_HERMIT_TOOL_ROOT": "/home/newton/work/dev-hermit",
            "RUSTUP_TOOLCHAIN": "nightly",
            "XDG_RUNTIME_DIR": f"/run/user/{os.getuid()}",
        }
        and service_command[: len(clean_service_prefix)] == clean_service_prefix
        and service_command[len(clean_service_prefix)]
        == launcher.WORKER_BOOTSTRAP
        and service_command.count("--runtime-capsule") == 1
        and service_command[systemd_capsule_index + 1] == "fixture-runtime-capsule"
        and service_command.count("--runtime-capsule-sha256") == 1
        and service_command[systemd_capsule_digest_index + 1]
        == launcher.runtime_capsule_sha256("fixture-runtime-capsule")
        and "%" not in launcher.WORKER_BOOTSTRAP,
        "systemd transport clears ambient state, installs the exact worker environment, and preserves isolated argv",
    )
    clean_tool_probe = subprocess.run(
        [
            "/usr/bin/env",
            "-i",
            *(
                f"{name}={value}"
                for name, value in exact_worker_environment.items()
            ),
            "/bin/sh",
            "-c",
            "test \"$HOME\" = /home/newton && "
            "test \"$(command -v cargo)\" = /home/newton/.cargo/bin/cargo && "
            "test \"$(command -v rust-script)\" = /home/newton/.cargo/bin/rust-script && "
            "case \"$(rustc --version)\" in *nightly*) : ;; *) exit 1 ;; esac && "
            "cargo --version >/dev/null && rust-script --version >/dev/null",
        ],
        text=True,
        capture_output=True,
        check=False,
    )
    require(
        clean_tool_probe.returncode == 0,
        "clean run6 worker environment resolves cargo, rust-script, and the nightly toolchain",
    )
    require(
        max(len(os.fsencode(argument)) for argument in constructed_systemd)
        <= 32 * os.sysconf("SC_PAGE_SIZE") - 1,
        "every systemd worker argument stays within Linux MAX_ARG_STRLEN",
    )
    truncation_marker = (
        b"\n=== HERMIT LOG TRUNCATED: reached the configured size bound "
        b"(HERMIT_LOG_MAX_BYTES). Output beyond this point was DISCARDED. "
        b"The run itself continued and was NOT affected. ===\n"
    )
    require(
        len(truncation_marker) == 176
        and launcher.PROCESS_FILE_LIMIT_BYTES
        == launcher.SERVICE_LOG_MAX_BYTES + len(truncation_marker),
        "RLIMIT_FSIZE leaves the exact 176-byte Hermit truncation-marker headroom",
    )
    preflight_capture: dict[str, object] = {}

    class SuccessfulPreflight:
        returncode = 0
        pid = 999_999

        def __init__(self, command, **kwargs):
            preflight_capture["command"] = command
            preflight_capture["kwargs"] = kwargs

        def communicate(self, timeout=None):
            preflight_capture["timeout"] = timeout
            return ("Usage: ci-hub validate-lock [OPTIONS]", "")

    with mock.patch.object(launcher.subprocess, "Popen", SuccessfulPreflight):
        launcher.run_validate_lock_preflight()
    preflight_kwargs = preflight_capture["kwargs"]
    require(
        preflight_capture["command"]
        == [
            "/usr/bin/with-proxy",
            "/home/newton/work/dev-hermit/ci-hub/ci-hub",
            "validate-lock",
            "--help",
        ]
        and preflight_kwargs["cwd"] == ROOT
        and preflight_kwargs["env"] == launcher.worker_environment()
        and preflight_kwargs["start_new_session"] is True
        and preflight_capture["timeout"] == 1_800,
        "bounded pre-record validate-lock preflight uses only the exact clean nightly worker environment",
    )

    with tempfile.TemporaryDirectory(prefix="kvm-ratchet-worker-bootstrap-") as bootstrap_tmp:
        bootstrap_root = Path(bootstrap_tmp)
        marker = bootstrap_root / "reviewed-launcher-ran"
        launcher_path = bootstrap_root / "launch.py"
        reviewed_source = (
            "from pathlib import Path\n"
            f"Path({str(marker)!r}).write_text('reviewed')\n"
            "raise SystemExit(0)\n"
        ).encode()
        launcher_path.write_bytes(reviewed_source)
        reviewed_source_sha = digest_bytes(reviewed_source)
        freeze_bytes = b"reviewed frozen manifest\n"
        freeze_digest = digest_bytes(freeze_bytes)
        completed_record = bootstrap_root / "completed.json"
        completed_record.write_text(
            json.dumps(
                {
                    "kind": "bench",
                    "state": "completed",
                    "unit": f"{launcher.UNIT}.service",
                    "target": SOURCE_SHA,
                }
            )
        )
        worker_prefix = [
            "/usr/bin/python3",
            "-I",
            "-S",
            "-c",
            launcher.WORKER_BOOTSTRAP,
            str(completed_record),
            f"{launcher.UNIT}.service",
            SOURCE_SHA,
        ]
        captured_worker_command = worker_prefix + [
            launcher.launcher_capsule(reviewed_source),
            reviewed_source_sha,
            freeze_digest,
            __import__("base64").b64encode(freeze_bytes).decode("ascii"),
            str(launcher_path),
            str(bootstrap_root),
        ]
        launcher_path.write_text("raise SystemExit('replacement pathname executed')\n")
        captured_worker = subprocess.run(
            captured_worker_command,
            text=True,
            capture_output=True,
            check=False,
        )
        require(
            captured_worker.returncode == 0 and marker.read_text() == "reviewed",
            "worker executes captured launcher bytes after its pathname is replaced",
        )

        failed_record = bootstrap_root / "failed.json"
        failed_record.write_text(json.dumps(trusted_launcher.initial_record()) + "\n")
        bad_capsule = subprocess.run(
            worker_prefix[:5]
            + [
                str(failed_record),
                f"{launcher.UNIT}.service",
                SOURCE_SHA,
                launcher.launcher_capsule(reviewed_source),
                "0" * 64,
                freeze_digest,
                __import__("base64").b64encode(freeze_bytes).decode("ascii"),
                str(launcher_path),
                str(bootstrap_root),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        failed_document = json.loads(failed_record.read_text())
        require(
            bad_capsule.returncode != 0
            and failed_document["state"] == "failed"
            and failed_document["result"] == "failed"
            and failed_document["exit_code"] != 0,
            "bootstrap digest refusal atomically terminates the attributed run record",
        )
    with tempfile.TemporaryDirectory(prefix="kvm-ratchet-run-record-race-") as record_tmp:
        race_record = Path(record_tmp) / "current.json"
        race_log = Path(record_tmp) / "service.log"

        def finish_before_parent_returns(*_args, **_kwargs):
            if not race_record.exists():
                return subprocess.CompletedProcess([], 3, "", "")
            trusted_launcher.run_registry.update_record(
                race_record,
                **trusted_launcher.terminal_fields(1, "synthetic fast worker failure"),
            )
            return subprocess.CompletedProcess([], 0, "", "")

        with (
            mock.patch.object(trusted_launcher, "RECORD", race_record),
            mock.patch.object(trusted_launcher, "LOG", race_log),
            mock.patch.object(trusted_launcher, "EXPECTED_RUN_CHECKOUT", ROOT),
            mock.patch.object(trusted_launcher, "EXPECTED_RUN_CAMPAIGN", SCRIPT_DIR),
            mock.patch.object(
                trusted_launcher,
                "capture_runtime_input_bytes",
                return_value={name: b"fixture" for name in trusted_launcher.RUNTIME_INPUT_NAMES},
            ),
            mock.patch.object(
                trusted_launcher,
                "create_runtime_input_snapshots",
                return_value={"paths": {}, "fds": ()},
            ),
            mock.patch.object(trusted_launcher, "close_runtime_input_snapshots"),
            mock.patch.object(trusted_launcher, "check_inputs", return_value={"ok": True}),
            mock.patch.object(
                trusted_launcher,
                "systemd_command",
                return_value=["fake-systemd-run"],
            ),
            mock.patch.object(trusted_launcher, "run_validate_lock_preflight"),
            mock.patch.object(
                trusted_launcher.subprocess,
                "run",
                side_effect=finish_before_parent_returns,
            ),
        ):
            launch_rc = trusted_launcher.launch(dry_run=False)
            terminal_after_parent = trusted_launcher.run_registry.read_record(race_record)
        require(
            launch_rc == 0
            and terminal_after_parent["state"] == "failed"
            and terminal_after_parent["result"] == "failed",
            "fast terminal worker record cannot be regressed to running by launcher parent",
        )
    with tempfile.TemporaryDirectory(prefix="kvm-ratchet-final-resource-") as resource_tmp:
        resource_root = Path(resource_tmp)
        resource_results = resource_root / "evidence"
        resource_results.mkdir()
        resource_log = resource_root / "service.log"
        resource_log.write_bytes(b"bounded service output\n")
        with (
            mock.patch.object(trusted_launcher, "RESULTS", resource_results),
            mock.patch.object(trusted_launcher, "LOG", resource_log),
        ):
            require(
                trusted_launcher.require_final_resource_headroom()["service_log_bytes"]
                == resource_log.stat().st_size,
                "launcher accepts a bounded service log and ample final disk headroom",
            )
            with resource_log.open("wb") as stream:
                stream.truncate(trusted_launcher.SERVICE_LOG_MAX_BYTES)
            try:
                trusted_launcher.require_final_resource_headroom()
            except RuntimeError:
                pass
            else:
                raise AssertionError("launcher accepted a service log at its hard cap")

            def gate_bytes(resources: dict[str, int]) -> bytes:
                return (
                    "WORKER_FINAL_RESOURCE_GATE "
                    + json.dumps(resources, sort_keys=True, separators=(",", ":"))
                    + "\n"
                ).encode("utf-8")

            def boundary_resources(final_slack: int) -> tuple[dict[str, int], bytes]:
                resources = {
                    "logical_bytes": 0,
                    "allocated_bytes": 0,
                    "filesystem_free_bytes": trusted_launcher.FILESYSTEM_RESERVE_BYTES,
                    "service_log_bytes": 0,
                }
                for _ in range(8):
                    encoded = gate_bytes(resources)
                    next_size = (
                        trusted_launcher.SERVICE_LOG_MAX_BYTES
                        - len(encoded)
                        - final_slack
                    )
                    if resources["service_log_bytes"] == next_size:
                        return resources, encoded
                    resources["service_log_bytes"] = next_size
                raise AssertionError("final resource-gate line length did not stabilize")

            allowed_resources, allowed_gate = boundary_resources(1)
            with resource_log.open("wb") as stream:
                stream.truncate(allowed_resources["service_log_bytes"])
            with resource_log.open("a", encoding="utf-8", newline="") as stream:
                with redirect_stdout(stream):
                    trusted_launcher.emit_final_resource_gate(allowed_resources)
            require(
                resource_log.stat().st_size
                == trusted_launcher.SERVICE_LOG_MAX_BYTES - 1
                and len(allowed_gate) > 0,
                "final resource-gate line fits only when the strict service-log cap retains one byte of headroom",
            )

            rejected_resources, rejected_gate = boundary_resources(0)
            with resource_log.open("wb") as stream:
                stream.truncate(rejected_resources["service_log_bytes"])
            rejected_before = resource_log.stat().st_size
            try:
                with resource_log.open("a", encoding="utf-8", newline="") as stream:
                    with redirect_stdout(stream):
                        trusted_launcher.emit_final_resource_gate(rejected_resources)
            except RuntimeError:
                pass
            else:
                raise AssertionError(
                    "launcher accepted a final resource-gate line ending exactly at the service-log cap"
                )
            require(
                resource_log.stat().st_size == rejected_before
                and rejected_before + len(rejected_gate)
                == trusted_launcher.SERVICE_LOG_MAX_BYTES,
                "final resource-gate cap-minus-line boundary fails before writing any bytes",
            )

            resource_log.write_bytes(b"pre-terminal resource log\n")
            post_write_resources = trusted_launcher.require_final_resource_headroom()
            post_write_gate = gate_bytes(post_write_resources)
            post_write_before = resource_log.stat().st_size
            post_write_error = None
            with (
                mock.patch.object(
                    trusted_launcher.os,
                    "statvfs",
                    return_value=mock.Mock(
                        f_bavail=trusted_launcher.FILESYSTEM_RESERVE_BYTES - 1,
                        f_frsize=1,
                    ),
                ),
                resource_log.open("a", encoding="utf-8", newline="") as stream,
                redirect_stdout(stream),
            ):
                try:
                    trusted_launcher.emit_final_resource_gate(post_write_resources)
                except RuntimeError as error:
                    post_write_error = str(error)
            require(
                post_write_error is not None
                and "128 GiB reserve" in post_write_error
                and resource_log.stat().st_size
                == post_write_before + len(post_write_gate),
                "post-flush resource recheck catches a gate-line allocation that crosses the filesystem reserve",
            )

        resource_record = resource_root / "current.json"
        with (
            mock.patch.object(trusted_launcher, "RECORD", resource_record),
            mock.patch.object(trusted_launcher, "RESULTS", resource_results),
            mock.patch.object(trusted_launcher, "LOG", resource_log),
            mock.patch.object(trusted_launcher, "EXPECTED_RUN_CHECKOUT", ROOT),
            mock.patch.object(trusted_launcher, "EXPECTED_RUN_CAMPAIGN", SCRIPT_DIR),
            mock.patch.object(
                trusted_launcher,
                "prepare_worker_snapshot",
                return_value=(
                    {
                        "source_sha": SOURCE_SHA,
                        "source_tree": SOURCE_TREE,
                        "payload_sha256": "0" * 64,
                        "freeze_manifest_sha256": "f" * 64,
                    },
                    {"paths": {}, "fds": ()},
                ),
            ),
            mock.patch.object(trusted_launcher, "close_runtime_input_snapshots"),
            mock.patch.object(trusted_launcher, "worker_command", return_value=["/bin/true"]),
            mock.patch.object(
                trusted_launcher,
                "read_successful_qualification",
                return_value={
                    "qualified_cell_count": 0,
                    "projected_overlap_numerator": 221,
                },
            ),
            mock.patch.object(
                trusted_launcher,
                "require_final_resource_headroom",
                return_value={
                    "logical_bytes": 0,
                    "allocated_bytes": 0,
                    "filesystem_free_bytes": trusted_launcher.FILESYSTEM_RESERVE_BYTES,
                    "service_log_bytes": 0,
                },
            ),
            mock.patch.object(
                trusted_launcher,
                "emit_final_resource_gate",
                side_effect=RuntimeError(
                    "attributed service log lacks headroom for its final resource gate"
                ),
            ),
        ):
            trusted_launcher.run_registry.create_current_record(
                resource_record, trusted_launcher.initial_record()
            )
            resource_worker_rc = trusted_launcher.run_worker(
                resource_record,
                launcher_sha256=trusted_launcher.EXECUTED_LAUNCHER_SHA256,
                freeze_manifest_sha256=trusted_launcher.EXECUTED_FREEZE_MANIFEST_SHA256,
                runtime_capsule="synthetic",
                runtime_capsule_digest=trusted_launcher.runtime_capsule_sha256(
                    "synthetic"
                ),
            )
            resource_terminal = trusted_launcher.run_registry.read_record(resource_record)
        require(
            resource_worker_rc != 0
            and resource_terminal["state"] == "failed"
            and resource_terminal["result"] == "failed",
            "final resource-gate write refusal publishes failed, never completed",
        )
    require(invocation_gate("yes", "yes", 0, 0), "driver advances a strict artifact-valid rc-zero invocation")
    require(
        not invocation_gate("yes", "yes", 7, 0),
        "driver rejects a strict PASS row followed by nonzero harness exit",
    )
    require(
        not invocation_gate("yes", "yes", 0, 1),
        "driver rejects disagreement with the recomputed harness exit",
    )
    require(
        not invocation_gate("yes", "no", 0, 0),
        "driver rejects a strict row with incomplete artifacts",
    )
    require(
        not invocation_gate("yes", "yes", 0, 0, 1),
        "driver rejects a strict row with an invocation-local infrastructure fault",
    )
    require(
        not invocation_gate("yes", "yes", 0, 0, 0, 1),
        "driver rejects a strict PASS invocation that leaked a private summary",
    )
    require(
        DRIVER.read_text().count("timeout --kill-after=10s 600s") == 2,
        "preparation and KVM outer invocations retain the 600s infrastructure backstop",
    )

    with tempfile.TemporaryDirectory(prefix="kvm-ratchet-resource-guard-") as guard_tmp:
        guard_root = Path(guard_tmp)
        scan_root = guard_root / "scan"
        scan_root.mkdir()
        service_log = guard_root / "service.log"
        service_log.write_bytes(b"")
        guard_header = (
            "timestamp_epoch_seconds\tscope\treason\tobserved_bytes\t"
            "limit_bytes\tpath\n"
        )
        guard_ledger = guard_root / "resource-guard.tsv"
        guard_ledger.write_text(guard_header)

        def reset_guard() -> None:
            guard_ledger.write_text(guard_header)
            (guard_root / ".resource-guard-tripped").unlink(missing_ok=True)

        def resource_guard(*, budget=1_000_000, reserve=0, cap=1024, service_cap=1024):
            return subprocess.run(
                [
                    "bash",
                    str(DRIVER),
                    "--self-test-resource-guard",
                    str(guard_root),
                    str(scan_root),
                    str(budget),
                    str(reserve),
                    str(cap),
                    str(cap),
                    str(cap),
                    str(service_log),
                    str(service_cap),
                ],
                text=True,
                capture_output=True,
                check=False,
            )

        require(
            resource_guard().returncode == 0,
            "resource guard accepts evidence below all fixed thresholds",
        )
        invocation_log = scan_root / "invocation.log"
        invocation_log.write_bytes(b"x" * 11)
        reset_guard()
        require(
            resource_guard(cap=10).returncode != 0
            and (guard_root / ".resource-guard-tripped").is_file()
            and "invocation-log-cap" in guard_ledger.read_text(),
            "resource guard records and refuses an oversized invocation log",
        )
        invocation_log.unlink()

        captures = scan_root / "run/captures"
        captures.mkdir(parents=True)
        capture = captures / "verify-1.stdout"
        capture.write_bytes(b"x" * 11)
        reset_guard()
        require(
            resource_guard(cap=10).returncode != 0
            and "capture-file-cap" in guard_ledger.read_text(),
            "resource guard records and refuses an oversized captured stream",
        )
        capture.unlink()

        retained_dir = scan_root / "run/verify-logs/verify-1"
        retained_dir.mkdir(parents=True)
        retained = retained_dir / "run1_log_fixture"
        retained.write_bytes(b"x" * 11)
        reset_guard()
        require(
            resource_guard(cap=10).returncode != 0
            and "retained-log-cap" in guard_ledger.read_text(),
            "resource guard records and refuses an oversized retained verify log",
        )
        retained.unlink()

        service_log.write_bytes(b"x" * 10)
        reset_guard()
        require(
            resource_guard(service_cap=10).returncode != 0
            and "service-log-cap" in guard_ledger.read_text(),
            "resource guard covers the attributed service log outside evidence",
        )
        service_log.write_bytes(b"")

        allocated = scan_root / "allocated"
        allocated.write_bytes(b"x" * 65_536)
        physical_bytes = sum(
            path.stat().st_blocks * 512
            for path in guard_root.rglob("*")
            if path.is_file()
        )
        reset_guard()
        require(
            resource_guard(budget=physical_bytes - 1).returncode != 0
            and "campaign-budget" in guard_ledger.read_text(),
            "resource guard rejects aggregate physical allocation above its budget",
        )
        allocated.unlink()

        reset_guard()
        require(
            resource_guard(reserve=9_223_372_036_854_775_807).returncode != 0
            and "filesystem-reserve" in guard_ledger.read_text(),
            "resource guard preserves the independent physical free-space reserve",
        )

        sparse = scan_root / "sparse"
        with sparse.open("wb") as stream:
            stream.truncate(1_000_000)
        reset_guard()
        require(
            resource_guard(budget=100_000).returncode != 0
            and "campaign-logical-budget" in guard_ledger.read_text(),
            "logical campaign budget rejects sparse or compressed apparent growth",
        )
        sparse.unlink()

        reset_guard()
        watchdog_output = scan_root / "invocation.log"
        descendant_pid_file = guard_root / "descendant.pid"
        watchdog = subprocess.run(
            [
                "bash",
                str(DRIVER),
                "--self-test-resource-watchdog",
                str(guard_root),
                str(scan_root),
                str(watchdog_output),
                "1000000",
                "0",
                "10",
                "1024",
                "1024",
                str(service_log),
                "1024",
                "--",
                "/bin/bash",
                "-c",
                "(trap '' TERM; printf '%s' \"$BASHPID\" > \"$1\"; "
                "while :; do printf 12345678901234567890; sleep 0.1; done) & wait",
                "watchdog-fixture",
                str(descendant_pid_file),
            ],
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )
        require(
            watchdog.returncode != 0
            and "invocation-log-cap" in guard_ledger.read_text()
            and (guard_root / ".resource-guard-tripped").is_file()
            and watchdog_output.stat().st_size > 10,
            "nominal one-second watchdog records a durable trip and preserves the output prefix",
        )
        future_producer_marker = guard_root / "future-producer-ran"
        refused_future_producer = subprocess.run(
            [
                "bash",
                str(DRIVER),
                "--self-test-resource-watchdog",
                str(guard_root),
                str(scan_root),
                str(scan_root / "future.log"),
                "1000000",
                "0",
                "1024",
                "1024",
                "1024",
                str(service_log),
                "1024",
                "--",
                "/usr/bin/touch",
                str(future_producer_marker),
            ],
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )
        require(
            refused_future_producer.returncode != 0
            and not future_producer_marker.exists(),
            "a durable resource trip refuses every later producer before execution",
        )
        descendant_pid = int(descendant_pid_file.read_text())
        descendant_alive = True
        for _ in range(40):
            if not Path(f"/proc/{descendant_pid}").exists():
                descendant_alive = False
                break
            time.sleep(0.05)
        require(
            not descendant_alive,
            "resource watchdog escalates TERM to KILL and leaves no process-group descendant",
        )

        reset_guard()
        generic_limit = subprocess.run(
            [
                "bash",
                str(DRIVER),
                "--self-test-resource-watchdog",
                str(guard_root),
                str(scan_root),
                str(scan_root / "build.log"),
                "1000000",
                "0",
                "1024",
                "1024",
                "1024",
                str(service_log),
                "1024",
                "--",
                "/bin/bash",
                "-c",
                "exit 153",
            ],
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )
        require(
            generic_limit.returncode != 0
            and (guard_root / ".resource-guard-tripped").is_file()
            and "process-file-limit" in guard_ledger.read_text(),
            "generic phase SIGXFSZ becomes a durable global resource failure",
        )

        monitor_stop = subprocess.run(
            ["bash", str(DRIVER), "--self-test-monitor-stop"],
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )
        require(
            monitor_stop.returncode == 0,
            "campaign-lifetime resource monitor is deterministically stopped and waited",
        )

    with tempfile.TemporaryDirectory(
        prefix="kvm-ratchet-initial-disk-snapshot-"
    ) as snapshot_tmp:
        snapshot_root = Path(snapshot_tmp)
        snapshot_probe = subprocess.run(
            [
                "bash",
                str(DRIVER),
                "--self-test-initial-disk-snapshot",
                str(snapshot_root),
            ],
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )
        probe_calls = Counter(
            (snapshot_root / "probe-calls").read_text().splitlines()
        )
        snapshot_fields = (snapshot_root / "disk-budget.tsv").read_text().splitlines()[
            1
        ].split("\t")
        snapshot_environment = (snapshot_root / "environment.log").read_text()
        require(
            snapshot_probe.returncode == 0
            and probe_calls
            == Counter(
                {
                    "allocated": 1,
                    "logical": 1,
                    "free": 1,
                    "service": 1,
                    "hits": 1,
                }
            )
            and snapshot_fields[:5]
            == ["initial", "4096", "2048", "1099511627776", "123"]
            and snapshot_fields[8:] == ["0", "clear"]
            and "initial_campaign_allocated_bytes=4096\n"
            in snapshot_environment
            and "initial_filesystem_free_bytes=1099511627776\n"
            in snapshot_environment
            and all(
                stale not in snapshot_environment
                for stale in ("8192", "549755813888")
            ),
            "initial disk-budget and environment records reuse one changing-probe tuple",
        )

    driver_source = DRIVER.read_text()
    require(
        '"$rlimit_soft" == 1073742000' in driver_source
        and '"$rlimit_hard" == 1073742000' in driver_source
        and "retained_log_truncation_marker_bytes=176" in driver_source,
        "pinned environment preserves the 1 GiB log plus exact 176-byte marker under RLIMIT_FSIZE",
    )
    if launcher_only:
        print("PASS launcher-only suite intentionally deferred row/artifact/freeze checks")
        print("PASS no build/Hermit/KVM/systemd action was executed")
        return 0

    with tempfile.TemporaryDirectory(prefix="kvm-ratchet-90-self-test-") as temporary:
        evidence = materialize_evidence_fixture(Path(temporary), cells, kernel)
        validated = validate_evidence(evidence)
        if validated.returncode != 0:
            raise AssertionError(f"complete adaptive fixture rejected: {validated.stdout}\n{validated.stderr}")
        validated_document = json.loads(validated.stdout)
        all_results_path = evidence / "all-results.jsonl"
        current_results = all_results_path.read_text()
        stale_results = current_results.replace(
            PREFIX,
            "kvm-ratchet-90-086d41a2",
            1,
        )
        require(
            stale_results != current_results,
            "run6 evidence fixture contains a campaign-prefix discriminator",
        )
        all_results_path.write_text(stale_results)
        require(
            validate_evidence(evidence).returncode != 0,
            "stale run5-era run-id prefix cannot validate as run6 evidence",
        )
        all_results_path.write_text(current_results)
        require(
            validate_evidence(evidence).returncode == 0,
            "restored run6 prefix retains the complete positive evidence fixture",
        )
        fixture_rows = [
            json.loads(line)
            for line in all_results_path.read_text().splitlines()
        ]
        result_census = Counter(row["result"] for row in fixture_rows)
        timeout_rows = [row for row in fixture_rows if row["result"] == "timeout"]
        killed_timeout_identities = {
            (row["test"], row["run_index"], row["attempt"])
            for row in timeout_rows
            if row["attempts"][0]["status"] is None
            and row["attempts"][0]["signal"] in (9, 15)
        }
        post_exit_cpu_timeout_identities = {
            (row["test"], row["run_index"], row["attempt"])
            for row in timeout_rows
            if row["error_kind"] == "cpu-timeout"
            and row["attempts"][0]["status"] in (0, 1, 125)
            and row["attempts"][0]["signal"] is None
        }
        pre_attempt_timeout_identities = {
            (row["test"], row["run_index"], row["attempt"])
            for row in timeout_rows
            if row["attempts"][0]["status"] is None
            and row["attempts"][0]["signal"] is None
        }
        timeout_identity_set = {
            (row["test"], row["run_index"], row["attempt"])
            for row in timeout_rows
        }
        no_report_timeout_identities = {
            (row["test"], row["run_index"], row["attempt"])
            for row in timeout_rows
            if row["attempts"][0]["verification_report"] is None
        }
        require(validated_document["ok"] is True, "complete mixed-outcome campaign evidence is accepted")
        require(validated_document["observed_invocation_count"] == 123, "two advancers produce 119 + 4 ordered invocations")
        require(validated_document["observed_result_rows"] == 240, "mixed fixture preserves all 240 attempt rows")
        require(validated_document["retry_rows"] == 117, "all 117 excluded cells retain deterministic retries")
        require(
            result_census
            == Counter(
                {
                    "timeout": 228,
                    "pass": 7,
                    "determinism-failure": 3,
                    "crash-error": 2,
                }
            ),
            "fixture result census independently partitions all 240 rows",
        )
        require(
            len(killed_timeout_identities) == 215
            and len(post_exit_cpu_timeout_identities) == 11
            and len(pre_attempt_timeout_identities) == 2
            and killed_timeout_identities.isdisjoint(post_exit_cpu_timeout_identities)
            and killed_timeout_identities.isdisjoint(pre_attempt_timeout_identities)
            and post_exit_cpu_timeout_identities.isdisjoint(pre_attempt_timeout_identities)
            and (
                killed_timeout_identities
                | post_exit_cpu_timeout_identities
                | pre_attempt_timeout_identities
            )
            == timeout_identity_set,
            "fixture independently partitions 228 timeouts into 215 killed, 11 post-exit CPU, and 2 pre-attempt rows",
        )
        require(
            validated_document["timeout_rows"] == len(timeout_identity_set)
            and validated_document["killed_timeout_rows"]
            == len(killed_timeout_identities)
            and validated_document["post_exit_cpu_timeout_rows"]
            == len(post_exit_cpu_timeout_identities)
            and validated_document["pre_attempt_timeout_rows"]
            == len(pre_attempt_timeout_identities),
            "validator timeout census equals the independently derived fixture identity sets",
        )
        require(
            len(no_report_timeout_identities) == 2
            and no_report_timeout_identities <= killed_timeout_identities
            and validated_document["executed_timeout_without_report_rows"]
            == len(no_report_timeout_identities),
            "two executed no-report rows are an explicit subset of killed timeouts",
        )
        require(validated_document["qualified_cell_count"] == 2, "two complete three-of-three strict cells qualify")
        require(validated_document["projected_overlap_numerator"] == 223, "projected selected-set overlap is 221 + 2")
        require(
            "same-backend KVM canonical L2 repeatability" in validated_document["qualification_scope"],
            "authoritative qualification is labeled same-backend KVM repeatability",
        )
        require(
            "not ptrace-vs-KVM" in validated_document["projected_overlap_scope"],
            "projected overlap is not labeled cross-backend output parity",
        )

        def require_resource_rejection(result, label: str) -> None:
            document = json.loads(result.stdout)
            require(
                result.returncode != 0
                and document["ok"] is False
                and "qualified_ids" not in document
                and "qualified_cell_count" not in document,
                label,
            )

        resource_ledger = evidence / "resource-guard.tsv"
        original_resource_ledger = resource_ledger.read_text()
        resource_ledger.write_text(
            original_resource_ledger
            + f"1\tfixture\tinvocation-log-cap\t1073741824\t1073741824\t{evidence}/results/example/invocation.log\n"
        )
        require_resource_rejection(
            validate_evidence(evidence),
            "a durable resource-guard trip globally invalidates evidence and exposes no qualification",
        )
        resource_ledger.write_text(original_resource_ledger)

        disk_budget = evidence / "disk-budget.tsv"
        original_disk_budget = disk_budget.read_text()
        disk_lines = original_disk_budget.splitlines(keepends=True)
        final_disk_fields = disk_lines[-1].rstrip("\n").split("\t")
        final_disk_fields[2] = "137438953473"
        disk_lines[-1] = "\t".join(final_disk_fields) + "\n"
        disk_budget.write_text("".join(disk_lines))
        require_resource_rejection(
            validate_evidence(evidence),
            "final logical bytes above 128 GiB globally invalidate evidence",
        )
        disk_budget.write_text(original_disk_budget)

        disk_lines = original_disk_budget.splitlines(keepends=True)
        final_disk_fields = disk_lines[-1].rstrip("\n").split("\t")
        final_disk_fields[3] = "137438953471"
        disk_lines[-1] = "\t".join(final_disk_fields) + "\n"
        disk_budget.write_text("".join(disk_lines))
        require_resource_rejection(
            validate_evidence(evidence),
            "final free space below the independent 128 GiB reserve invalidates evidence",
        )
        disk_budget.write_text(original_disk_budget)

        pinned_environment = evidence / "pinned-environment.log"
        original_pinned_environment = pinned_environment.read_text()
        mutated_pinned_environment = original_pinned_environment.replace(
            "container_rlimit_fsize_hard_bytes=1073742000\n", ""
        )
        require(
            mutated_pinned_environment != original_pinned_environment,
            "container RLIMIT mutation changed the fixture bytes",
        )
        pinned_environment.write_text(mutated_pinned_environment)
        require_resource_rejection(
            validate_evidence(evidence),
            "missing container RLIMIT_FSIZE inheritance evidence invalidates the campaign",
        )
        pinned_environment.write_text(original_pinned_environment)

        exercise_leaked_summary_discriminators(evidence, cells)

        pass_result_dir = (
            evidence
            / "results"
            / cells[0]["test"].replace("/", "-")
            / "repetition-1"
        )
        pass_leak_dir = pass_result_dir / "leaked-private-summaries"
        pass_leak = pass_leak_dir / ".hermit-verify-summary-pass-fixture"
        pass_leak.write_text("impossible pass leak\n")
        pass_manifest = pass_result_dir / "leaked-summaries.tsv"
        original_pass_manifest = pass_manifest.read_text()
        pass_manifest.write_text(
            original_pass_manifest
            + f"verify\t{pass_leak.relative_to(evidence)}\t"
            f"{int(pass_leak.stat().st_mtime)}\t{pass_leak.stat().st_size}\t"
            f"{digest_file(pass_leak)}\n"
        )
        invocation_ledger = evidence / "results/invocations.tsv"
        original_invocation_ledger = invocation_ledger.read_text()
        invocation_lines = original_invocation_ledger.splitlines(keepends=True)
        for index, line in enumerate(invocation_lines[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[0] == cells[0]["test"] and fields[2] == "1":
                fields[7] = "1"
                invocation_lines[index] = "\t".join(fields) + "\n"
                break
        invocation_ledger.write_text("".join(invocation_lines))
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "an otherwise strict PASS with an attributed leak invalidates evidence and cannot qualify",
        )
        pass_leak.unlink()
        pass_manifest.write_text(original_pass_manifest)
        invocation_ledger.write_text(original_invocation_ledger)
        rebuild_artifact_inventory(evidence)

        payload_ledger = evidence / "payload-input-checks.tsv"
        original_payload_ledger = payload_ledger.read_text()
        payload_ledger.write_text(original_payload_ledger.replace(driver_sha, "0" * 64, 1))
        require(
            validate_evidence(evidence).returncode != 0,
            "payload phase ledger rejects a startup hash mismatch",
        )
        payload_ledger.write_text(original_payload_ledger)

        invocation_records = [
            line.rstrip("\n").split("\t")
            for line in (evidence / "results/invocations.tsv").read_text().splitlines()
        ][1:]
        ordered_result_files = [Path(fields[8]) / "results.jsonl" for fields in invocation_records]
        original_result_bytes = {
            path: path.read_bytes() for path in ordered_result_files
        }
        original_all_results = (evidence / "all-results.jsonl").read_bytes()
        original_artifacts = (evidence / "artifacts.tsv").read_text()
        original_binary_ledger = (evidence / "built-binary.tsv").read_text()
        forged_binary_sha = "0" * 64
        result_hashes: dict[str, str] = {}
        rebuilt_rows: list[bytes] = []
        for result_file in ordered_result_files:
            rows = [
                json.loads(line)
                for line in result_file.read_text().splitlines()
                if line
            ]
            for row in rows:
                row["binary_sha256"] = forged_binary_sha
            write_jsonl(result_file, rows)
            result_hashes[str(result_file)] = digest_file(result_file)
            rebuilt_rows.append(result_file.read_bytes())
        (evidence / "all-results.jsonl").write_bytes(b"".join(rebuilt_rows))
        forged_artifact_lines = original_artifacts.splitlines(keepends=True)
        for index, line in enumerate(forged_artifact_lines[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            fields[4] = result_hashes[fields[3]]
            forged_artifact_lines[index] = "\t".join(fields) + "\n"
        (evidence / "artifacts.tsv").write_text("".join(forged_artifact_lines))
        (evidence / "built-binary.tsv").write_text(
            "sha256\tpath\n"
            f"{forged_binary_sha}\tsplit/target/release/hermit\n"
        )
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "coordinated forged row and ledger binary digest disagrees with the retained executable",
        )
        for result_file, content in original_result_bytes.items():
            result_file.write_bytes(content)
        (evidence / "all-results.jsonl").write_bytes(original_all_results)
        (evidence / "artifacts.tsv").write_text(original_artifacts)
        (evidence / "built-binary.tsv").write_text(original_binary_ledger)
        rebuild_artifact_inventory(evidence)

        eligible_ledger = evidence / "round1-eligible.tsv"
        original_eligible_ledger = eligible_ledger.read_text()
        eligible_ledger.write_text("test\tselector\n")
        require(
            validate_evidence(evidence).returncode != 0,
            "deleted adaptive eligibility decisions fail closed",
        )
        eligible_ledger.write_text(original_eligible_ledger)

        final_status_rc = evidence / "source-status.after.rc"
        final_status_rc.write_text("1\n")
        require(
            validate_evidence(evidence).returncode != 0,
            "failed final git-status command cannot validate as a clean checkout",
        )
        final_status_rc.write_text("0\n")

        initial_status_rc = evidence / "source-status.before.rc"
        initial_status_rc.write_text("1\n")
        require(
            validate_evidence(evidence).returncode != 0,
            "failed initial git-status command cannot validate as a clean checkout",
        )
        initial_status_rc.write_text("0\n")

        first_pass_result = next(
            path
            for path in evidence.glob("results/*/repetition-1/results.jsonl")
            if json.loads(path.read_text().splitlines()[0])["outcome"] == "PASS"
        )
        strict_artifacts = subprocess.run(
            ["bash", str(STRICT_ARTIFACT_VALIDATOR), str(first_pass_result.parent), str(first_pass_result)],
            text=True,
            capture_output=True,
            check=False,
        )
        require(strict_artifacts.returncode == 0, "round advancement requires intact strict-pass artifacts")

        pass_log_dir = next(first_pass_result.parent.glob("runs/*/*/verify-logs/verify-1"))
        extra_pass_log = pass_log_dir / "unexpected_extra"
        extra_pass_log.write_text("unledgered\n")
        strict_artifacts_with_extra = subprocess.run(
            ["bash", str(STRICT_ARTIFACT_VALIDATOR), str(first_pass_result.parent), str(first_pass_result)],
            text=True,
            capture_output=True,
            check=False,
        )
        require(strict_artifacts_with_extra.returncode != 0, "round advancement rejects an extra retained log")
        extra_pass_log.unlink()

        pass_row = json.loads(first_pass_result.read_text().splitlines()[0])
        pass_artifact = (
            first_pass_result.parent
            / str(pass_row["artifact_dir"]).removeprefix("/results/")
        )
        mandatory_paths = (
            (first_pass_result.parent / "invocation.log", "invocation.log"),
            (first_pass_result.parent / "junit.xml", "junit.xml"),
            (first_pass_result.parent / "summary.json", "summary.json"),
            (pass_artifact / "captures/verify-1.stdout", "stdout capture"),
            (pass_artifact / "captures/verify-1.stderr", "stderr capture"),
        )
        for mandatory_path, mandatory_label in mandatory_paths:
            mandatory_bytes = mandatory_path.read_bytes()
            mandatory_path.unlink()
            rebuild_artifact_inventory(evidence)
            require(
                validate_evidence(evidence).returncode != 0,
                f"missing mandatory {mandatory_label} fails closed",
            )
            mandatory_path.write_bytes(mandatory_bytes)
            rebuild_artifact_inventory(evidence)

        stdout_capture = pass_artifact / "captures/verify-1.stdout"
        original_stdout_capture = stdout_capture.read_bytes()
        stdout_capture.write_bytes(b"contradicts embedded stdout\n")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "capture bytes must equal embedded attempt stdout/stderr",
        )
        stdout_capture.write_bytes(original_stdout_capture)
        rebuild_artifact_inventory(evidence)

        invocation_log = first_pass_result.parent / "invocation.log"
        invocation_log_mode = invocation_log.stat().st_mode & 0o777
        invocation_log.chmod(0)
        require(
            validate_evidence(evidence).returncode != 0,
            "unhashable mandatory invocation log invalidates the inventory",
        )
        invocation_log.chmod(invocation_log_mode)

        extra_symlink = evidence / "results/unexpected-symlink"
        extra_symlink.symlink_to(invocation_log)
        require(
            validate_evidence(evidence).returncode != 0,
            "a symlink anywhere in the retained results tree fails closed",
        )
        extra_symlink.unlink()

        extra_fifo = evidence / "results/unexpected-fifo"
        os.mkfifo(extra_fifo)
        require(
            validate_evidence(evidence).returncode != 0,
            "a special node anywhere in the retained results tree fails closed",
        )
        extra_fifo.unlink()

        artifact_inventory = evidence / "artifact-hashes.tsv"
        original_inventory = artifact_inventory.read_text()
        first_inventory_row = original_inventory.splitlines(keepends=True)[1]
        artifact_inventory.write_text(original_inventory + first_inventory_row)
        require(
            validate_evidence(evidence).returncode != 0,
            "duplicate artifact inventory paths fail closed",
        )
        artifact_inventory.write_text(original_inventory)

        artifact_ledger = evidence / "artifacts.tsv"
        original_artifact_ledger = artifact_ledger.read_text()
        artifact_lines = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(artifact_lines[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[6] == "pre-attempt-not-run":
                fields[6] = "executed-report"
                artifact_lines[index] = "\t".join(fields) + "\n"
                break
        else:
            raise AssertionError("fixture has no pre-attempt artifact row")
        artifact_ledger.write_text("".join(artifact_lines))
        require(
            validate_evidence(evidence).returncode != 0,
            "pre-attempt artifact state cannot be relabeled as executed",
        )
        artifact_ledger.write_text(original_artifact_ledger)

        pre_attempt_fields = next(
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[6] == "pre-attempt-not-run"
        )
        pre_attempt_result_dir = Path(pre_attempt_fields[3]).parent
        pre_attempt_host_artifact = (
            pre_attempt_result_dir / pre_attempt_fields[5].removeprefix("/results/")
        )
        unexpected_pre_attempt_report = pre_attempt_host_artifact / "verify-1.json"
        unexpected_pre_attempt_report.write_text("{}")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "pre-attempt timeout rejects an unexpected disk report",
        )
        unexpected_pre_attempt_report.unlink()
        rebuild_artifact_inventory(evidence)
        pre_attempt_log_dir = pre_attempt_host_artifact / "verify-logs/verify-1"
        pre_attempt_log_dir.rmdir()
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "pre-attempt timeout requires its producer-created empty log directory",
        )
        pre_attempt_log_dir.mkdir()
        rebuild_artifact_inventory(evidence)

        pre_attempt_summary = pre_attempt_result_dir / "summary.json"
        pre_attempt_summary_document = json.loads(pre_attempt_summary.read_text())
        require(
            pre_attempt_summary_document["cell_cpu_usage_usec"] is None,
            "pre-attempt retry history propagates unknown CPU into summary.json",
        )
        pre_attempt_summary_document["cell_cpu_usage_usec"] = 0
        pre_attempt_summary.write_text(
            json.dumps(pre_attempt_summary_document, indent=2, sort_keys=True) + "\n"
        )
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "summary cannot invent CPU usage for a pre-attempt timeout history",
        )
        pre_attempt_summary_document["cell_cpu_usage_usec"] = None
        pre_attempt_summary.write_text(
            json.dumps(pre_attempt_summary_document, indent=2, sort_keys=True) + "\n"
        )
        rebuild_artifact_inventory(evidence)

        no_report_fields = [
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[6] == "executed-no-report"
        ]
        require(
            len(no_report_fields) == 2
            and all(fields[7:11] == ["-", "-", "-", "-"] for fields in no_report_fields)
            and sum(fields[11] != "-" for fields in no_report_fields) == 1,
            "executed no-report timeout records absent target/logs and an optional exact staging file",
        )
        no_report_relabel = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(no_report_relabel[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[6] == "executed-no-report":
                fields[6] = "executed-report"
                no_report_relabel[index] = "\t".join(fields) + "\n"
                break
        artifact_ledger.write_text("".join(no_report_relabel))
        require(
            validate_evidence(evidence).returncode != 0,
            "executed no-report timeout cannot be relabeled as report-backed",
        )
        artifact_ledger.write_text(original_artifact_ledger)

        no_report_artifact = (
            Path(no_report_fields[0][3]).parent
            / no_report_fields[0][5].removeprefix("/results/")
        )
        impossible_no_report_file = no_report_artifact / "verify-1.json"
        impossible_no_report_file.write_text("{}")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "null-report timeout rejects an impossible final report",
        )
        impossible_no_report_file.unlink()
        rebuild_artifact_inventory(evidence)

        no_report_log_dir = no_report_artifact / "verify-logs/verify-1"
        impossible_no_report_log = no_report_log_dir / "run1_log_impossible"
        impossible_no_report_log.write_bytes(b"")
        no_report_hybrid = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(no_report_hybrid[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[:3] == no_report_fields[0][:3]:
                fields[9] = str(impossible_no_report_log)
                no_report_hybrid[index] = "\t".join(fields) + "\n"
                break
        artifact_ledger.write_text("".join(no_report_hybrid))
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "null-report timeout rejects an impossible partial-log hybrid",
        )
        artifact_ledger.write_text(original_artifact_ledger)
        impossible_no_report_log.unlink()
        rebuild_artifact_inventory(evidence)

        no_report_staged_fields = next(fields for fields in no_report_fields if fields[11] != "-")
        no_report_staging = Path(no_report_staged_fields[11])
        require(
            no_report_staged_fields[7] == "-"
            and re.fullmatch(r"\.tmp[A-Za-z0-9]{6}", no_report_staging.name) is not None
            and no_report_staging.is_file(),
            "signal-killed initial report publication may retain one exact staged file without a target",
        )
        reordered_artifacts = original_artifact_ledger.splitlines(keepends=True)
        staged_line_index = next(
            index
            for index, line in enumerate(reordered_artifacts[1:], start=1)
            if line.rstrip("\n").split("\t")[:3] == no_report_staged_fields[:3]
        )
        staged_line = reordered_artifacts.pop(staged_line_index)
        reordered_artifacts.insert(1, staged_line)
        artifact_ledger.write_text("".join(reordered_artifacts))
        require(
            validate_evidence(evidence).returncode == 0,
            "a valid staged timeout is accepted both first and after other artifact states",
        )
        artifact_ledger.write_text(original_artifact_ledger)

        pending_staged_fields = next(
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[0] == cells[5]["test"]
            and line.rstrip("\n").split("\t")[2] == "1"
        )
        require(
            pending_staged_fields[7] != "-"
            and pending_staged_fields[11] != "-"
            and json.loads(Path(pending_staged_fields[7]).read_text())["no_result_reason"]
            == {"kind": "not_run"},
            "signal-killed later publication may retain a staged file beside the pending target",
        )

        def ledger_with_staging(fields: list[str], path: Path) -> str:
            lines = original_artifact_ledger.splitlines(keepends=True)
            for index, line in enumerate(lines[1:], start=1):
                candidate = line.rstrip("\n").split("\t")
                if candidate[:3] == fields[:3]:
                    candidate[11] = str(path)
                    lines[index] = "\t".join(candidate) + "\n"
                    return "".join(lines)
            raise AssertionError("staging mutation row is absent")

        pass_fields = next(
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[:3]
            == [cells[0]["test"], "1", "1"]
        )
        pass_stage = pass_artifact / ".tmpP4s5A6"
        pass_stage.write_bytes(b"impossible pass staging bytes\n")
        artifact_ledger.write_text(ledger_with_staging(pass_fields, pass_stage))
        rebuild_artifact_inventory(evidence)
        strict_pass_with_stage = subprocess.run(
            [
                "bash",
                str(STRICT_ARTIFACT_VALIDATOR),
                str(first_pass_result.parent),
                str(first_pass_result),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        require(
            strict_pass_with_stage.returncode != 0
            and validate_evidence(evidence).returncode != 0,
            "strict PASS with atomic report staging neither advances nor validates",
        )
        pass_stage.unlink()
        artifact_ledger.write_text(original_artifact_ledger)

        normal_timeout_fields = next(
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[0] == cells[3]["test"]
        )
        normal_timeout_artifact = (
            Path(normal_timeout_fields[3]).parent
            / normal_timeout_fields[5].removeprefix("/results/")
        )
        normal_timeout_stage = normal_timeout_artifact / ".tmpN0rM4l"
        normal_timeout_stage.write_bytes(b"impossible normal-exit timeout stage\n")
        artifact_ledger.write_text(
            ledger_with_staging(normal_timeout_fields, normal_timeout_stage)
        )
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "normal-exit timeout rejects an atomic report staging file",
        )
        normal_timeout_stage.unlink()
        artifact_ledger.write_text(original_artifact_ledger)

        terminal_signal_fields = next(
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[0] == cells[8]["test"]
        )
        terminal_signal_artifact = (
            Path(terminal_signal_fields[3]).parent
            / terminal_signal_fields[5].removeprefix("/results/")
        )
        terminal_signal_stage = terminal_signal_artifact / ".tmpT3rM1n"
        terminal_signal_stage.write_bytes(b"impossible terminal report stage\n")
        artifact_ledger.write_text(
            ledger_with_staging(terminal_signal_fields, terminal_signal_stage)
        )
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "signal-killed timeout with a terminal matched report rejects staging residue",
        )
        terminal_signal_stage.unlink()
        artifact_ledger.write_text(original_artifact_ledger)

        first_run_signal_fields = next(
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
            if line.rstrip("\n").split("\t")[:3]
            == [cells[6]["test"], "1", "1"]
        )
        first_run_signal_artifact = (
            Path(first_run_signal_fields[3]).parent
            / first_run_signal_fields[5].removeprefix("/results/")
        )
        first_run_signal_stage = first_run_signal_artifact / ".tmpF1r5tR"
        first_run_signal_stage.write_bytes(b"impossible first-run terminal stage\n")
        artifact_ledger.write_text(
            ledger_with_staging(first_run_signal_fields, first_run_signal_stage)
        )
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "signal-killed timeout with a first-run-rejected report rejects staging residue",
        )
        first_run_signal_stage.unlink()
        artifact_ledger.write_text(original_artifact_ledger)

        malformed_stage = no_report_staging.with_name(".tmpBAD-xx")
        no_report_staging.rename(malformed_stage)
        artifact_ledger.write_text(
            ledger_with_staging(no_report_staged_fields, malformed_stage)
        )
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "atomic report staging residue rejects a non-producer basename",
        )
        malformed_stage.rename(no_report_staging)
        artifact_ledger.write_text(original_artifact_ledger)

        second_stage = no_report_staging.with_name(".tmpZ9y8X7")
        second_stage.write_bytes(b"second impossible staging file\n")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "one attempt cannot retain multiple atomic report staging files",
        )
        second_stage.unlink()
        rebuild_artifact_inventory(evidence)

        stage_hardlink = evidence.parent / "atomic-stage-hardlink-alias"
        os.link(no_report_staging, stage_hardlink)
        require(
            validate_evidence(evidence).returncode != 0,
            "atomic report staging residue rejects a hardlink alias",
        )
        stage_hardlink.unlink()

        no_report_unstaged_fields = next(fields for fields in no_report_fields if fields[11] == "-")
        no_report_unstaged_artifact = (
            Path(no_report_unstaged_fields[3]).parent
            / no_report_unstaged_fields[5].removeprefix("/results/")
        )
        staging_symlink = no_report_unstaged_artifact / ".tmpS1y2M3"
        staging_symlink.symlink_to(no_report_staging)
        artifact_ledger.write_text(
            ledger_with_staging(no_report_unstaged_fields, staging_symlink)
        )
        require(
            validate_evidence(evidence).returncode != 0,
            "atomic report staging residue rejects a symlink",
        )
        staging_symlink.unlink()
        artifact_ledger.write_text(original_artifact_ledger)

        arbitrary_root_file = pass_artifact / "unexpected-root-file"
        arbitrary_root_file.write_bytes(b"unexpected\n")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "artifact root rejects an arbitrary extra file",
        )
        arbitrary_root_file.unlink()
        rebuild_artifact_inventory(evidence)

        ledger_rows = [
            line.rstrip("\n").split("\t")
            for line in original_artifact_ledger.splitlines(keepends=True)[1:]
        ]
        pending_timeout_fields = [
            fields
            for fields in ledger_rows
            if fields[0] in (cells[5]["test"], cells[12]["test"])
        ]
        first_run_timeout_fields = [
            fields for fields in ledger_rows if fields[0] == cells[6]["test"]
        ]
        pending_timeout_log_shapes = {
            (fields[9] != "-", fields[10] != "-")
            for fields in pending_timeout_fields
        }
        require(
            pending_timeout_log_shapes
            == {(False, False), (True, False), (True, True)},
            "pending NotRun timeout fixtures cover {}, {run1}, and {run1,run2}",
        )
        first_run_timeout_log_shapes = {
            (fields[9] != "-", fields[10] != "-")
            for fields in first_run_timeout_fields
        }
        require(
            first_run_timeout_log_shapes == {(True, False), (True, True)},
            "timeout first-run-rejected fixtures require run1 and allow run2",
        )

        zero_log_fields = next(
            fields
            for fields in pending_timeout_fields
            if fields[9] == "-" and fields[10] == "-"
        )
        zero_log_artifact = (
            Path(zero_log_fields[3]).parent
            / zero_log_fields[5].removeprefix("/results/")
        )
        impossible_run2 = zero_log_artifact / "verify-logs/verify-1/run2_log_impossible"
        impossible_run2.write_bytes(b"")
        run2_only_ledger = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(run2_only_ledger[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[:3] == zero_log_fields[:3]:
                fields[10] = str(impossible_run2)
                run2_only_ledger[index] = "\t".join(fields) + "\n"
                break
        artifact_ledger.write_text("".join(run2_only_ledger))
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "timeout artifact ledger rejects impossible run2-without-run1 order",
        )
        artifact_ledger.write_text(original_artifact_ledger)
        impossible_run2.unlink()
        rebuild_artifact_inventory(evidence)

        run1_only_fields = next(
            fields
            for fields in pending_timeout_fields
            if fields[9] != "-" and fields[10] == "-"
        )
        run1_only_path = Path(run1_only_fields[9])
        unexpected_timeout_log = run1_only_path.parent / "unexpected_extra"
        unexpected_timeout_log.write_bytes(b"")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "timeout partial log set rejects an extra entry",
        )
        unexpected_timeout_log.unlink()
        rebuild_artifact_inventory(evidence)

        wrong_name_path = run1_only_path.parent / "wrong_name"
        run1_only_path.rename(wrong_name_path)
        wrong_name_ledger = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(wrong_name_ledger[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[:3] == run1_only_fields[:3]:
                fields[9] = str(wrong_name_path)
                wrong_name_ledger[index] = "\t".join(fields) + "\n"
                break
        artifact_ledger.write_text("".join(wrong_name_ledger))
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "timeout partial log set rejects a wrong run-log name",
        )
        wrong_name_path.rename(run1_only_path)
        artifact_ledger.write_text(original_artifact_ledger)
        rebuild_artifact_inventory(evidence)

        both_log_fields = next(
            fields
            for fields in first_run_timeout_fields
            if fields[9] != "-" and fields[10] != "-"
        )
        alias_ledger = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(alias_ledger[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[:3] == both_log_fields[:3]:
                fields[10] = fields[9]
                alias_ledger[index] = "\t".join(fields) + "\n"
                break
        artifact_ledger.write_text("".join(alias_ledger))
        require(
            validate_evidence(evidence).returncode != 0,
            "timeout partial log ledger rejects aliased run1/run2 paths",
        )
        artifact_ledger.write_text(original_artifact_ledger)

        status125_first_run_fields = next(
            fields
            for fields in first_run_timeout_fields
            if fields[2] == "2"
        )
        require(
            status125_first_run_fields[9] != "-"
            and status125_first_run_fields[10] == "-",
            "post-exit status-125 first-run rejection retains exactly run1",
        )
        impossible_status125_run2 = (
            Path(status125_first_run_fields[9]).parent / "run2_log_impossible"
        )
        impossible_status125_run2.write_bytes(b"")
        status125_hybrid = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(status125_hybrid[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[:3] == status125_first_run_fields[:3]:
                fields[10] = str(impossible_status125_run2)
                status125_hybrid[index] = "\t".join(fields) + "\n"
                break
        artifact_ledger.write_text("".join(status125_hybrid))
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "post-exit status-125 first-run rejection rejects an impossible run2 log",
        )
        impossible_status125_run2.unlink()
        artifact_ledger.write_text(original_artifact_ledger)
        rebuild_artifact_inventory(evidence)

        nonzero_timeout_fields = next(
            fields for fields in ledger_rows if fields[0] == cells[7]["test"]
        )
        nonzero_timeout_log = Path(nonzero_timeout_fields[9])
        original_nonzero_timeout_log = nonzero_timeout_log.read_bytes()
        nonzero_timeout_log.write_bytes(b"")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "nonzero canonical timeout counts require nonempty retained logs",
        )
        nonzero_timeout_log.write_bytes(original_nonzero_timeout_log)
        rebuild_artifact_inventory(evidence)

        asymmetric_timeout_fields = next(
            fields for fields in ledger_rows if fields[0] == cells[9]["test"]
        )
        asymmetric_run1 = Path(asymmetric_timeout_fields[9])
        asymmetric_run2 = Path(asymmetric_timeout_fields[10])
        require(
            asymmetric_run1.stat().st_size == 0
            and asymmetric_run2.stat().st_size > 0,
            "zero-left timeout retains an empty left log and nonempty right log",
        )
        original_asymmetric_run2 = asymmetric_run2.read_bytes()
        asymmetric_run2.write_bytes(b"")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "positive right-side timeout count requires a nonempty run2 log",
        )
        asymmetric_run2.write_bytes(original_asymmetric_run2)
        rebuild_artifact_inventory(evidence)

        zero_count_timeout_fields = next(
            fields for fields in ledger_rows if fields[0] == cells[10]["test"]
        )
        require(
            Path(zero_count_timeout_fields[9]).stat().st_size == 0
            and Path(zero_count_timeout_fields[10]).stat().st_size == 0,
            "zero-count canonical timeout may retain two empty logs",
        )
        zero_count_run1 = Path(zero_count_timeout_fields[9])
        zero_count_run1.write_bytes(b"WARN retained non-INFO or partial record\n")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode == 0,
            "zero selected INFO count also permits nonempty non-INFO log bytes",
        )
        zero_count_run1.write_bytes(b"")
        rebuild_artifact_inventory(evidence)

        zero_match_timeout_fields = next(
            fields for fields in ledger_rows if fields[0] == cells[13]["test"]
        )
        require(
            Path(zero_match_timeout_fields[9]).stat().st_size == 0
            and Path(zero_match_timeout_fields[10]).stat().st_size == 0,
            "matched zero-count timeout retains two explicit empty logs",
        )

        invocation_path = evidence / "results/invocations.tsv"
        original_invocations = invocation_path.read_text()
        invocation_lines = original_invocations.splitlines(keepends=True)
        for index, line in enumerate(invocation_lines[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[3] == "1":
                fields[3] = "0"
                invocation_lines[index] = "\t".join(fields) + "\n"
                break
        else:
            raise AssertionError("fixture has no excluded invocation for rc discriminator")
        invocation_path.write_text("".join(invocation_lines))
        rebuild_artifact_inventory(evidence)
        require(validate_evidence(evidence).returncode != 0, "ledger rc cannot relabel a failed terminal attempt as success")
        invocation_path.write_text(original_invocations)
        rebuild_artifact_inventory(evidence)

        ordinary_no_result_fields = next(
            fields for fields in ledger_rows if fields[0] == cells[2]["test"]
        )
        no_result_report = Path(ordinary_no_result_fields[7])
        no_result_run1 = Path(ordinary_no_result_fields[9])
        original_no_result_run1 = no_result_run1.read_bytes()
        no_result_run1.write_bytes(b"")
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode == 0,
            "executed no_result may retain an empty run1 log",
        )
        no_result_run1.write_bytes(original_no_result_run1)
        rebuild_artifact_inventory(evidence)

        no_result_run2 = no_result_run1.parent / "run2_log_fixture"
        no_result_run2.write_bytes(b"")
        ledger_with_run2 = original_artifact_ledger.splitlines(keepends=True)
        for index, line in enumerate(ledger_with_run2[1:], start=1):
            fields = line.rstrip("\n").split("\t")
            if fields[7] == str(no_result_report):
                fields[10] = str(no_result_run2)
                ledger_with_run2[index] = "\t".join(fields) + "\n"
                break
        else:
            raise AssertionError("no_result report is absent from the artifact ledger")
        artifact_ledger.write_text("".join(ledger_with_run2))
        rebuild_artifact_inventory(evidence)
        require(
            validate_evidence(evidence).returncode != 0,
            "non-timeout first-run-rejected evidence rejects an impossible run2 log",
        )
        artifact_ledger.write_text(original_artifact_ledger)
        no_result_run2.unlink()
        rebuild_artifact_inventory(evidence)

        extra_no_result_log = no_result_report.parent / "verify-logs/verify-1/unexpected_extra"
        extra_no_result_log.write_text("unledgered\n")
        rebuild_artifact_inventory(evidence)
        require(validate_evidence(evidence).returncode != 0, "no_result log directory rejects an arbitrary extra file")
        extra_no_result_log.unlink()
        rebuild_artifact_inventory(evidence)

        report_path = next(evidence.glob("results/*/repetition-1/runs/*/*/verify-1.json"))
        report_path.write_text(report_path.read_text() + "tamper")
        tampered = validate_evidence(evidence)
        require(tampered.returncode != 0, "verification-report/artifact hash tamper fails closed")
        tampered_document = json.loads(tampered.stdout)
        require(
            "qualified_cell_count" not in tampered_document
            and "qualified_ids" not in tampered_document,
            "invalid evidence exposes only provisional row-level qualification",
        )

    # Run exact freeze-chain checks last so an intentionally open candidate can
    # exercise every substantive evidence discriminator with --semantic-only
    # before the owner authorizes the final acyclic hash binding.
    if semantic_only:
        print("PASS semantic-only suite intentionally deferred freeze-chain checks")
        print("PASS no build/Hermit/KVM/systemd action was executed")
        return 0

    launcher_sha = digest_file(LAUNCHER)
    freeze_manifest_sha = digest_file(FREEZE_MANIFEST)
    frozen_runtime_inputs = trusted_launcher.capture_runtime_input_bytes(
        freeze_manifest_source=FREEZE_MANIFEST.read_bytes(),
        freeze_manifest_sha256=freeze_manifest_sha,
    )
    frozen_runtime_capsule = trusted_launcher.encode_runtime_input_capsule(
        frozen_runtime_inputs
    )
    frozen_worker_command = trusted_launcher.worker_command(
        runtime_capsule=frozen_runtime_capsule,
        runtime_capsule_digest=trusted_launcher.runtime_capsule_sha256(
            frozen_runtime_capsule
        ),
        payload_sha256=trusted_launcher.FROZEN_HASHES[DRIVER.name],
        freeze_manifest_sha256=freeze_manifest_sha,
    )
    frozen_systemd_command = trusted_launcher.systemd_command(
        launcher_sha256=launcher_sha,
        freeze_manifest_sha256=freeze_manifest_sha,
        freeze_manifest_source=FREEZE_MANIFEST.read_bytes(),
        launcher_source=LAUNCHER.read_bytes(),
        runtime_capsule=frozen_runtime_capsule,
    )
    frozen_worker_vector = trusted_launcher.exec_vector_measurement(
        frozen_worker_command,
        dict(os.environ),
    )
    frozen_systemd_vector = trusted_launcher.exec_vector_measurement(
        frozen_systemd_command,
        dict(os.environ),
    )
    print(
        "FROZEN_EXEC_VECTOR "
        + json.dumps(
            {
                "runtime_capsule_bytes": len(frozen_runtime_capsule.encode("ascii")),
                "runtime_capsule_sha256": trusted_launcher.runtime_capsule_sha256(
                    frozen_runtime_capsule
                ),
                "systemd": frozen_systemd_vector,
                "validate_lock_worker": frozen_worker_vector,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    require(
        frozen_worker_vector["max_argument_bytes"]
        <= frozen_worker_vector["max_argument_limit"]
        and frozen_worker_vector["total_bytes"] < frozen_worker_vector["arg_max"]
        and frozen_systemd_vector["max_argument_bytes"]
        <= frozen_systemd_vector["max_argument_limit"]
        and frozen_systemd_vector["total_bytes"]
        < frozen_systemd_vector["arg_max"],
        "final frozen systemd and validate-lock argv/environment vectors fit Linux exec limits",
    )
    worker_receipt = trusted_launcher.verify_worker_snapshot(
        launcher_sha256=launcher_sha,
        freeze_manifest_sha256=freeze_manifest_sha,
        runtime_capsule=frozen_runtime_capsule,
        runtime_capsule_digest=trusted_launcher.runtime_capsule_sha256(
            frozen_runtime_capsule
        ),
    )
    require(
        worker_receipt["source_sha"] == SOURCE_SHA
        and worker_receipt["source_tree"] == SOURCE_TREE
        and worker_receipt["payload_sha256"]
        == trusted_launcher.FROZEN_HASHES[DRIVER.name]
        and worker_receipt["runtime_capsule_sha256"]
        == trusted_launcher.runtime_capsule_sha256(frozen_runtime_capsule),
        "post-systemd worker revalidates exact source and frozen payload",
    )
    worker_tamper_rejected = False
    try:
        trusted_launcher.verify_worker_snapshot(
            launcher_sha256="0" * 64,
            freeze_manifest_sha256=freeze_manifest_sha,
            runtime_capsule=frozen_runtime_capsule,
            runtime_capsule_digest=trusted_launcher.runtime_capsule_sha256(
                frozen_runtime_capsule
            ),
        )
    except RuntimeError:
        worker_tamper_rejected = True
    require(worker_tamper_rejected, "worker rejects launcher TOCTOU hash mismatch")
    runtime_capsule_digest_rejected = False
    try:
        trusted_launcher.verify_worker_snapshot(
            launcher_sha256=launcher_sha,
            freeze_manifest_sha256=freeze_manifest_sha,
            runtime_capsule=frozen_runtime_capsule,
            runtime_capsule_digest="0" * 64,
        )
    except RuntimeError:
        runtime_capsule_digest_rejected = True
    require(
        runtime_capsule_digest_rejected,
        "worker rejects systemd-to-worker runtime-capsule digest mismatch",
    )
    manifest_tamper_rejected = False
    try:
        trusted_launcher.verify_worker_snapshot(
            launcher_sha256=launcher_sha,
            freeze_manifest_sha256="0" * 64,
            runtime_capsule=frozen_runtime_capsule,
            runtime_capsule_digest=trusted_launcher.runtime_capsule_sha256(
                frozen_runtime_capsule
            ),
        )
    except RuntimeError:
        manifest_tamper_rejected = True
    require(manifest_tamper_rejected, "worker rejects freeze-manifest TOCTOU hash mismatch")

    print("PASS no build/Hermit/KVM/systemd action was executed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
