#!/usr/bin/env python3
"""Bracket validate.rs's historical-debug admission contract."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "scripts" / "validate.rs"
HISTORICAL = ROOT / "scripts" / "historical-debug-validate"


def run_validate(
    *args: str, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(VALIDATE), *args],
        cwd=ROOT,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=30,
    )


def run_historical(
    *args: str, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(HISTORICAL), *args],
        cwd=ROOT,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=30,
    )


def assert_refused(result: subprocess.CompletedProcess[str], message: str) -> None:
    assert result.returncode == 2, result.stdout
    assert message in result.stdout, result.stdout
    assert "running historical-debug" not in result.stdout, result.stdout


def find_adapter() -> Path:
    return next(
        candidate / "ci-hub" / "ledger" / "validate_rows.py"
        for candidate in ROOT.parents
        if (candidate / "ci-hub" / "ledger" / "validate_rows.py").is_file()
    )


def ledger_events(root: Path) -> list[dict]:
    paths = [
        *root.glob("ignored/ci-hub/validate-ledger-spool/*.jsonl"),
        *root.glob("ledger/hermit/*/*.jsonl"),
    ]
    return [json.loads(line) for path in paths for line in path.read_text().splitlines()]


def init_clean_checkout(path: Path) -> str:
    path.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=path, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=validate fixture",
            "-c",
            "user.email=validate-fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "fixture",
        ],
        cwd=path,
        check=True,
    )
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=path, text=True).strip()


def make_fake_parent(path: Path) -> Path:
    """Create an inert wrapper endpoint that execs only the requested payload."""
    ci_hub = path / "ci-hub" / "ci-hub"
    ci_hub.parent.mkdir(parents=True)
    (path / ".gitmodules").write_text(
        '[submodule "hermit"]\n\tpath = hermit\n\turl = https://example.invalid/hermit\n'
    )
    ci_hub.write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import os
            import sys

            args = sys.argv[1:]
            if args[:2] != ["validate-lock", "run"] or "--" not in args:
                raise SystemExit(2)
            separator = args.index("--")
            if args[args.index("--kind") + 1] != "bench":
                raise SystemExit(2)
            command = args[separator + 1 :]
            os.execvpe(command[0], command, os.environ.copy())
            """
        )
    )
    ci_hub.chmod(0o755)
    return ci_hub


def process_start_ticks(pid: int) -> int:
    text = Path(f"/proc/{pid}/stat").read_text()
    return int(text[text.rfind(")") + 1 :].split()[19])


def authority_status(kind: str, target: str) -> str:
    host = subprocess.check_output(["hostname", "-s"], text=True).strip()
    return json.dumps(
        {
            "schema_version": 1,
            "admissible": True,
            "state": "held",
            "reason_code": None,
            "canonical_anchor_held": True,
            "cleanup_state": "none",
            "holder": {"kind": kind, "target": target, "host": host},
            "owner": {
                "host": host,
                "liveness": "alive",
                "pid": os.getpid(),
                "start_ticks": process_start_ticks(os.getpid()),
                "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            },
        }
    )


def run_admission_fixture(
    parent: Path,
    checkout: Path,
    target: str,
    kind: str,
    validate_args: list[str],
    *,
    mode: str,
    historical_marker: bool = False,
    nested: bool = False,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update(
        DEV_HERMIT_PARENT=str(parent),
        HERMIT_VALIDATE_ADMISSION_TEST_MODE=mode,
        VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON=authority_status(kind, target),
    )
    if nested:
        # This Python process remains a live ancestor through the fake lock's
        # exec, reproducing the outer validate marker without starting a gate.
        env["HERMIT_VALIDATE_ACTIVE"] = str(os.getpid())
    if historical_marker:
        env["CI_HUB_HISTORICAL_DEBUG_PRODUCER"] = "validate-lock-bench-v1"
    return subprocess.run(
        [str(VALIDATE), *validate_args],
        cwd=checkout,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=30,
    )


def assert_live_admission_paths() -> None:
    """Exercise the real wrapper/front door and outer+nested decision path."""
    with tempfile.TemporaryDirectory(prefix="validate-admission-") as tmp:
        tmpdir = Path(tmp)
        checkout = tmpdir / "checkout"
        target = init_clean_checkout(checkout)
        parent = tmpdir / "dev-hermit"
        make_fake_parent(parent)

        wrapper_env = os.environ.copy()
        wrapper_env.update(
            DEV_HERMIT_PARENT=str(parent),
            HERMIT_VALIDATE_ADMISSION_TEST_MODE="front-door",
            VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON=authority_status("bench", target),
        )
        wrapped = run_historical(
            "--checkout",
            str(checkout),
            "--agent",
            "fixture",
            "--target",
            target,
            env=wrapper_env,
        )
        assert wrapped.returncode == 0, wrapped.stdout
        assert "accepted canonical bench ownership" in wrapped.stdout, wrapped.stdout

        normal_owner = run_admission_fixture(
            parent,
            checkout,
            target,
            "validate",
            ["portable-only", "--historical-debug", "--no-label-pr", "-j", "1"],
            mode="front-door",
            historical_marker=True,
        )
        assert normal_owner.returncode == 4, normal_owner.stdout
        assert "canonical ci-hub admission" in normal_owner.stdout, normal_owner.stdout

        normal = run_admission_fixture(
            parent,
            checkout,
            target,
            "validate",
            ["quick", "--no-label-pr"],
            mode="front-door",
        )
        assert normal.returncode == 0, normal.stdout
        assert "accepted canonical validate ownership" in normal.stdout, normal.stdout

        bench_for_normal = run_admission_fixture(
            parent,
            checkout,
            target,
            "bench",
            ["quick", "--no-label-pr"],
            mode="front-door",
        )
        assert bench_for_normal.returncode == 4, bench_for_normal.stdout

        current_target = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip()
        nested = run_admission_fixture(
            parent,
            ROOT,
            current_target,
            "validate",
            ["--portable-strict-compat-only", "--no-label-pr"],
            mode="nested-peer",
            nested=True,
        )
        assert nested.returncode == 0, nested.stdout
        assert "inherited the outer peer authority" in nested.stdout, nested.stdout


def assert_typed_nonqualifying_row() -> None:
    """The accepted path writes one row whose type itself prevents qualification."""
    with tempfile.TemporaryDirectory(prefix="validate-historical-") as tmp:
        test_root = Path(tmp)
        env = os.environ.copy()
        env.update(
            CI_HUB_HISTORICAL_DEBUG_PRODUCER="validate-lock-bench-v1",
            HERMIT_VALIDATE_STOP_TEST_MODE="1",
            VALIDATE_STOP_TEST_LEDGER_TOOL=str(find_adapter()),
            CI_HUB_VALIDATE_LEDGER_TEST_ROOT=str(test_root),
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            VALIDATE_STOP_TEST_TMP_ROOT=str(test_root / "validation"),
            DEV_HERMIT_PARENT=str(ROOT.parents[2]),
            TMPDIR=str(test_root),
        )
        result = run_validate(
            "portable-only", "--historical-debug", "--no-label-pr", "-j", "1", env=env
        )
        assert result.returncode == 1, result.stdout
        events = ledger_events(test_root)
        assert len(events) == 1, (events, result.stdout)
        event = events[0]
        assert event["schema"] == "validate-ledger/v1", event
        row = event["legacy_row"]
        assert row["schema_version"] == 5, row
        assert row["producer"] == "hermit-validate-rs", row
        assert row["profile"] == "portable-only", row
        assert row["selection_mode"] == "historical-debug", row
        assert row["evidence_class"] == "historical-debug", row
        assert row["landing_eligible"] is False, row
        assert row["non_qualifying_reason"] == "historical-debug", row
        assert row["admission"] is None, row


def main() -> None:
    help_result = run_validate("--help")
    assert help_result.returncode == 0, help_result.stdout
    assert "--historical-debug" in help_result.stdout, help_result.stdout
    assert "NON-QUALIFYING" in help_result.stdout, help_result.stdout

    parallel = run_validate("portable-only", "--historical-debug", "-j", "2")
    assert_refused(parallel, "sequential and requires -j 1")

    unboxed = run_validate("portable-only", "--historical-debug", "--allow-cgroup-failure")
    assert_refused(unboxed, "requires cgroup boxing")

    direct = run_validate("portable-only", "--historical-debug", "-j", "1")
    assert_refused(direct, "use ./scripts/historical-debug-validate")

    wrapper_help = run_historical("--help")
    assert wrapper_help.returncode == 0, wrapper_help.stdout
    assert "box-exclusive validate/bench lock" in wrapper_help.stdout
    assert "NON-QUALIFYING" in wrapper_help.stdout

    malformed_target = run_historical(
        "--checkout", str(ROOT), "--agent", "test", "--target", "not-a-sha"
    )
    assert malformed_target.returncode == 2, malformed_target.stdout
    assert "exact lowercase 40-hex SHA" in malformed_target.stdout

    assert_live_admission_paths()
    assert_typed_nonqualifying_row()
    print(
        "PASS: historical-debug bench admission and outer+nested peer inheritance are "
        "bracketed; historical rows remain serialized, sequential, boxed, and typed "
        "NON-QUALIFYING at the canonical adapter"
    )


if __name__ == "__main__":
    main()
