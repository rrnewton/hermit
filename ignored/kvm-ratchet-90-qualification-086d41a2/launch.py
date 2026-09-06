#!/usr/bin/env python3
"""Launch the frozen all-119 adaptive KVM campaign as an attributed bench run."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


STATE_ROOT = Path("/home/newton/work/dev-hermit")
TOOL_ROOT = STATE_ROOT
CAMPAIGN = Path(__file__).resolve().parent
CHECKOUT = CAMPAIGN.parents[1]
RESULTS = CAMPAIGN / "evidence"
PAYLOAD = CAMPAIGN / "run-kvm-ratchet-90.sh"
FREEZE_MANIFEST = CAMPAIGN / "FROZEN_SHA256SUMS"
TARGET = "086d41a2f2f76e5a2cccceea342feb6957311c2b"
TARGET_TREE = "94490fd2cee758395150590a8ba25ff86443dbfd"
AGENT = "review_hermit_teardown_final_run"
UNIT = "hermit-kvm-ratchet-90-086d41a2"
LOG = STATE_ROOT / "ignored/validate" / f"{UNIT}.log"
WAIT_SECONDS = 1_800
HOLD_SECONDS = 600
CHILD_DEADLINE_SECONDS = 7_200
UNIT_RUNTIME_SECONDS = WAIT_SECONDS + CHILD_DEADLINE_SECONDS + 300
DEADLINE_BASIS = (
    "prior 120 outer/130 attempt campaign=1169s; adaptive max=357 outer/595 attempts; "
    "retry-aware projection=5350s; 7200s is a deliberate ~1.35x operational "
    "stop-for-investigation covering fetch/build/119 serial preparations/finalization; "
    "each outer invocation retains a 600s infrastructure-only backstop above its "
    "~248s two-attempt modeled path"
)
FROZEN_HASHES = {
    "run-kvm-ratchet-90.sh": "03fbf736f8bff6a280dcf258314f0c4c1952b6f40192b24bd507e24ecfc36b63",
    "validate-evidence.sh": "8a5fbee7714966fb551ebbbb91c6b5fa437f3cd0b7aa1eb860977cd59bf7573c",
    "validate-results.jq": "6d214388c8e60c73950a4606d8145982a3a8e2a5278946680be56f6ac9e89fba",
    "validate-strict-invocation-artifacts.sh": "0230ae477c01a64c8aed105976f1b4f3af8f5ae36d1696849f3cc80d9d2388e3",
    "validate-population.jq": "7db7ead41b484473386b5abf93a84020186f821d691743deffe72bb94da62d2f",
    "expected-cells.json": "a8d9d8e83eb03d3757e2f87e3d0b88c0387e7f505b19e5b7987fbaf2ca2b4f26",
    "self-test.py": "8c7f0a4734e65e7073360d56648c54a8744850608f7f6262f57e5d5c6c37a44c",
}
PAYLOAD_GUARD = r'''set -euo pipefail
expected=$1
payload=$2
hash_line=$(/usr/bin/sha256sum -- "$payload")
actual=${hash_line%% *}
if [[ "$actual" != "$expected" ]]; then
  printf 'QUEUE_PAYLOAD_CHECK mismatch expected=%s actual=%s path=%s\n' \
    "$expected" "$actual" "$payload" >&2
  exit 75
fi
printf 'QUEUE_PAYLOAD_CHECK ok expected=%s actual=%s path=%s\n' \
  "$expected" "$actual" "$payload"
exec /usr/bin/env KVM_RATCHET_PAYLOAD_SHA256="$expected" /bin/bash "$payload"
'''

sys.path.insert(0, str(TOOL_ROOT / "ci-hub/validate"))
import run_registry  # noqa: E402

RECORD = run_registry.record_path(STATE_ROOT, UNIT)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def initial_record() -> dict[str, object]:
    return {
        "schema_version": run_registry.SCHEMA_VERSION,
        "kind": "bench",
        "state": "launching",
        "unit": f"{UNIT}.service",
        "target": TARGET,
        "repo": "rrnewton/hermit",
        "checkout": str(CHECKOUT),
        "log": str(LOG),
        "agent": AGENT,
        "started_at": utc_now(),
        "producer": run_registry.PRODUCER,
        "admission": "ci-hub validate-lock",
    }


def guarded_payload_command(payload: Path, expected_sha256: str) -> list[str]:
    return [
        "/bin/bash",
        "-c",
        PAYLOAD_GUARD,
        "kvm-ratchet-payload-guard",
        expected_sha256,
        str(payload),
    ]


def worker_command(*, payload_sha256: str) -> list[str]:
    return [
        str(TOOL_ROOT / "ci-hub/ci-hub"),
        "validate-lock",
        "run",
        "--agent",
        AGENT,
        "--kind",
        "bench",
        "--target",
        TARGET,
        "--run-record",
        str(RECORD),
        "--wait",
        str(WAIT_SECONDS),
        "--hold",
        str(HOLD_SECONDS),
        "--child-deadline",
        str(CHILD_DEADLINE_SECONDS),
        "--",
    ] + guarded_payload_command(PAYLOAD, payload_sha256)


def systemd_command(*, launcher_sha256: str, freeze_manifest_sha256: str) -> list[str]:
    home = os.environ.get("HOME", "")
    path = os.environ.get("PATH", "")
    if not home or not path:
        raise RuntimeError("HOME and PATH must be set for the systemd user unit")
    systemd_run = shutil.which("systemd-run")
    if systemd_run is None:
        raise RuntimeError("systemd-run is unavailable")
    with_proxy = shutil.which("with-proxy")
    if with_proxy is None:
        raise RuntimeError("with-proxy is unavailable")
    return [
        systemd_run,
        "--user",
        "--quiet",
        "--collect",
        f"--unit={UNIT}",
        "--description=qualify the exact 119-cell KVM complement at 086d41a2",
        f"--property=RuntimeMaxSec={UNIT_RUNTIME_SECONDS}",
        f"--property=StandardOutput=append:{LOG}",
        f"--property=StandardError=append:{LOG}",
        f"--working-directory={CHECKOUT}",
        f"--setenv=HOME={home}",
        f"--setenv=PATH={path}",
        "--setenv=PYTHONUNBUFFERED=1",
        f"--setenv=DEV_HERMIT_PARENT={STATE_ROOT}",
        f"--setenv=DEV_HERMIT_TOOL_ROOT={TOOL_ROOT}",
        "--",
        with_proxy,
        sys.executable,
        str(Path(__file__).resolve()),
        "_run",
        "--record",
        str(RECORD),
        "--launcher-sha256",
        launcher_sha256,
        "--freeze-manifest-sha256",
        freeze_manifest_sha256,
    ]


def print_paths() -> None:
    print(f"HANDLE {UNIT}.service")
    print(f"RECORD {RECORD}")
    print(f"LOG {LOG}")
    print(f"RESULTS {RESULTS}")
    print(f"CHILD_DEADLINE_SECONDS {CHILD_DEADLINE_SECONDS}")
    print(f"UNIT_RUNTIME_SECONDS {UNIT_RUNTIME_SECONDS}")
    print(f"DEADLINE_BASIS {DEADLINE_BASIS}")


def refuse_unstarted(detail: str, exit_code: int) -> None:
    run_registry.update_record(
        RECORD,
        state="refused",
        result="systemd-launch-refused",
        exit_code=exit_code,
        detail=detail,
        finished_at=utc_now(),
    )


def terminal_fields(exit_code: int, detail: str | None = None) -> dict[str, object]:
    if exit_code == 0:
        fields: dict[str, object] = {
            "state": "completed",
            "result": "passed",
            "exit_code": 0,
            "detail": "campaign evidence complete; qualification is recorded in strict-validation.json",
            "finished_at": utc_now(),
            "results": str(RESULTS),
        }
    else:
        fields = {
            "state": "failed",
            "result": "failed",
            "exit_code": exit_code,
            "detail": detail or "campaign evidence invalid or incomplete",
            "finished_at": utc_now(),
            "results": str(RESULTS),
        }
    # Keep this close to the producer: an invalid terminal vocabulary must be
    # refused before it can strand the durable bench record in a live state.
    run_registry.parse_current_record(initial_record() | fields)
    return fields


def check_freeze_manifest() -> str:
    if not FREEZE_MANIFEST.is_file():
        raise RuntimeError(f"freeze manifest is absent: {FREEZE_MANIFEST}")
    observed: dict[str, str] = {}
    for line_number, line in enumerate(FREEZE_MANIFEST.read_text().splitlines(), start=1):
        digest, separator, name = line.partition("  ")
        if (
            separator != "  "
            or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
            or not name
            or name in observed
        ):
            raise RuntimeError(f"malformed freeze manifest line {line_number}")
        observed[name] = digest
    expected = FROZEN_HASHES | {"launch.py": sha256(Path(__file__).resolve())}
    if observed != expected:
        raise RuntimeError("freeze manifest does not exactly bind launcher and payload inputs")
    for name, expected_hash in expected.items():
        actual = sha256(CAMPAIGN / name)
        if actual != expected_hash:
            raise RuntimeError(
                f"freeze manifest mismatch for {name}: expected {expected_hash}, got {actual}"
            )
    return sha256(FREEZE_MANIFEST)


def check_inputs() -> dict[str, object]:
    if not CHECKOUT.is_dir():
        raise RuntimeError(f"checkout is absent: {CHECKOUT}")
    check_freeze_manifest()
    for name, expected in FROZEN_HASHES.items():
        path = CAMPAIGN / name
        if not path.is_file():
            raise RuntimeError(f"frozen input is absent: {path}")
        actual = sha256(path)
        if actual != expected:
            raise RuntimeError(
                f"frozen input changed: {name}: expected {expected}, got {actual}"
            )
    head = subprocess.run(
        ["git", "-C", str(CHECKOUT), "rev-parse", "HEAD^{commit}"],
        capture_output=True,
        text=True,
        check=False,
    )
    observed_head = head.stdout.strip()
    if head.returncode != 0 or observed_head != TARGET:
        detail = head.stderr.strip() or observed_head or f"exit {head.returncode}"
        raise RuntimeError(f"checkout is not at exact target {TARGET}: {detail}")
    tree = subprocess.run(
        ["git", "-C", str(CHECKOUT), "rev-parse", "HEAD^{tree}"],
        capture_output=True,
        text=True,
        check=False,
    )
    observed_tree = tree.stdout.strip()
    if tree.returncode != 0 or observed_tree != TARGET_TREE:
        detail = tree.stderr.strip() or observed_tree or f"exit {tree.returncode}"
        raise RuntimeError(f"checkout is not at exact tree {TARGET_TREE}: {detail}")
    status = subprocess.run(
        [
            "git",
            "-C",
            str(CHECKOUT),
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if status.returncode != 0 or status.stdout:
        detail = status.stderr.strip() or status.stdout.strip() or f"exit {status.returncode}"
        raise RuntimeError(f"checkout is not clean: {detail}")
    static = subprocess.run(
        ["/bin/bash", str(PAYLOAD), "--static-check"],
        cwd=CHECKOUT,
        capture_output=True,
        text=True,
        check=False,
    )
    if static.returncode != 0:
        detail = static.stderr.strip() or static.stdout.strip() or f"exit {static.returncode}"
        raise RuntimeError(f"static population check failed: {detail}")
    try:
        document = json.loads(static.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"static population check emitted invalid JSON: {error}") from error
    if document.get("ok") is not True or document.get("mode") != (
        "static-only-no-build-no-hermit-no-kvm-no-systemd"
    ):
        raise RuntimeError("static population check did not return its exact success contract")
    return document


def static_check() -> int:
    document = check_inputs()
    print(json.dumps(document, sort_keys=True, separators=(",", ":")))
    return 0


def launch(*, dry_run: bool) -> int:
    check_inputs()
    record = initial_record()
    run_registry.parse_current_record(record)
    launcher_sha256 = sha256(Path(__file__).resolve())
    freeze_manifest_sha256 = sha256(FREEZE_MANIFEST)
    command = systemd_command(
        launcher_sha256=launcher_sha256,
        freeze_manifest_sha256=freeze_manifest_sha256,
    )

    if RECORD.exists():
        raise RuntimeError(f"run record already exists: {RECORD}")
    if LOG.exists():
        raise RuntimeError(f"run log already exists: {LOG}")

    if dry_run:
        print("DRY-RUN: no record, log, validate lock, or systemd unit was created")
        print_paths()
        print(f"COMMAND {shlex.join(command)}")
        return 0

    active = subprocess.run(
        ["systemctl", "--user", "is-active", "--quiet", f"{UNIT}.service"],
        capture_output=True,
        text=True,
        check=False,
    )
    if active.returncode == 0:
        raise RuntimeError(f"systemd unit is already active: {UNIT}.service")
    if active.returncode not in (3, 4):
        detail = active.stderr.strip() or active.stdout.strip() or f"exit {active.returncode}"
        raise RuntimeError(f"cannot establish systemd unit state: {detail}")

    run_registry.create_current_record(RECORD, record)
    try:
        run_registry.reserve_log(LOG)
    except Exception as error:
        refuse_unstarted(str(error), 1)
        raise

    try:
        started = subprocess.run(command, capture_output=True, text=True, check=False)
    except Exception as error:
        refuse_unstarted(f"systemd-run could not be invoked: {error}", 1)
        raise
    if started.returncode != 0:
        detail = started.stderr.strip() or started.stdout.strip() or f"exit {started.returncode}"
        refuse_unstarted(detail, started.returncode)
        raise RuntimeError(f"systemd-run refused service: {detail}")

    print("LAUNCHED: attributed adaptive KVM bench run accepted by systemd")
    print_paths()
    return 0


def verify_worker_snapshot(
    *, launcher_sha256: str, freeze_manifest_sha256: str
) -> dict[str, object]:
    observed_launcher_sha256 = sha256(Path(__file__).resolve())
    observed_freeze_manifest_sha256 = sha256(FREEZE_MANIFEST)
    if observed_launcher_sha256 != launcher_sha256:
        raise RuntimeError(
            "launcher changed between initial admission and the systemd worker"
        )
    if observed_freeze_manifest_sha256 != freeze_manifest_sha256:
        raise RuntimeError(
            "freeze manifest changed between initial admission and the systemd worker"
        )
    document = check_inputs()
    expected_payload_sha256 = FROZEN_HASHES[PAYLOAD.name]
    observed_payload_sha256 = sha256(PAYLOAD)
    if observed_payload_sha256 != expected_payload_sha256:
        raise RuntimeError(
            "payload changed after frozen-input validation: "
            f"expected {expected_payload_sha256}, got {observed_payload_sha256}"
        )
    return {
        "source_sha": document["source_sha"],
        "source_tree": document["source_tree"],
        "launcher_sha256": observed_launcher_sha256,
        "freeze_manifest_sha256": observed_freeze_manifest_sha256,
        "payload_sha256": expected_payload_sha256,
        "observed_payload_sha256": observed_payload_sha256,
        "payload_inputs": FROZEN_HASHES,
        "checked_at": utc_now(),
    }


def run_worker(
    record: Path, *, launcher_sha256: str, freeze_manifest_sha256: str
) -> int:
    if record.resolve() != RECORD.resolve():
        print(f"launcher: refusing unexpected record path: {record}", file=sys.stderr)
        return 2
    try:
        # The asynchronous worker owns the launching -> running transition.
        # The parent must never perform a late write that can regress a fast
        # terminal worker record back to running.
        run_registry.update_record(RECORD, state="running")
        receipt = verify_worker_snapshot(
            launcher_sha256=launcher_sha256,
            freeze_manifest_sha256=freeze_manifest_sha256,
        )
        print(
            "WORKER_FROZEN_INPUTS "
            + json.dumps(receipt, sort_keys=True, separators=(",", ":")),
            flush=True,
        )
        print(f"DEADLINE_BASIS {DEADLINE_BASIS}")
        completed = subprocess.run(
            worker_command(payload_sha256=str(receipt["payload_sha256"])),
            cwd=CHECKOUT,
            check=False,
        )
        exit_code = completed.returncode
        fields = terminal_fields(
            exit_code,
            f"campaign evidence invalid or incomplete: validate-lock or payload exited {exit_code}",
        )
        run_registry.update_record(RECORD, **fields)
        return exit_code
    except Exception as error:
        try:
            run_registry.update_record(RECORD, **terminal_fields(1, f"campaign evidence invalid: {error}"))
        except Exception as record_error:
            print(
                f"launcher: {error}; terminal run-record update also failed: {record_error}",
                file=sys.stderr,
            )
            return 1
        print(f"launcher: {error}", file=sys.stderr)
        return 1


def status() -> int:
    print_paths()
    if RECORD.exists():
        value = run_registry.read_record(RECORD)
        print("RUN_RECORD " + json.dumps(value, sort_keys=True, separators=(",", ":")))
    else:
        print("RUN_RECORD absent")
    observed = subprocess.run(
        [
            "systemctl",
            "--user",
            "show",
            f"{UNIT}.service",
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=Result",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
            "--no-pager",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    output = observed.stdout.strip()
    print("SYSTEMD " + (output.replace("\n", " ") if output else f"unavailable rc={observed.returncode}"))
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    launch_parser = commands.add_parser("launch")
    launch_parser.add_argument("--dry-run", action="store_true")
    worker = commands.add_parser("_run")
    worker.add_argument("--record", type=Path, required=True)
    worker.add_argument("--launcher-sha256", required=True)
    worker.add_argument("--freeze-manifest-sha256", required=True)
    commands.add_parser("status")
    commands.add_parser("static-check")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "launch":
            return launch(dry_run=args.dry_run)
        if args.command == "_run":
            return run_worker(
                args.record,
                launcher_sha256=args.launcher_sha256,
                freeze_manifest_sha256=args.freeze_manifest_sha256,
            )
        if args.command == "status":
            return status()
        if args.command == "static-check":
            return static_check()
        raise AssertionError(args.command)
    except RuntimeError as error:
        print(f"launcher: REFUSED: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
