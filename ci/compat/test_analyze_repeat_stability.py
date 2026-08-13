#!/usr/bin/env python3
"""Regression brackets for root-execution eligibility classification."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location(
    "analyze_repeat_stability", HERE / "analyze_repeat_stability.py"
)
assert SPEC and SPEC.loader
ANALYZER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYZER)

FAILURES: list[str] = []


def check(label: str, condition: bool) -> None:
    if condition:
        print(f"ok   {label}")
    else:
        print(f"FAIL {label}")
        FAILURES.append(label)


def legacy_label_matching_form(program: dict[str, object]) -> str:
    """The defective pre-fix predicate, retained only as a mutant control."""

    argv = [str(item) for item in program["argv"]]
    label = str(program["label"])
    if "real_compat_workload.sh" in " ".join(argv):
        return "workload-script"
    first = Path(argv[0]).name
    if first == label or (label == "bracket" and first == "["):
        return "named-program-direct"
    if label == "bash" and first == "bash":
        return "named-program-direct"
    return "shell-or-launcher-wrapped"


SCENARIO_DIRECTS = (
    {
        "label": "mpstat-softirqs",
        "argv": ["/usr/bin/mpstat", "-I", "SCPU", "1", "1"],
    },
    {
        "label": "pidstat-disk",
        "argv": ["/usr/bin/pidstat", "-d", "-p", "1", "1", "1"],
    },
    {"label": "sar-resource-tables", "argv": ["/usr/bin/sar", "-v", "1", "1"]},
    {
        "label": "sysctl-random-uuid",
        "argv": ["/usr/sbin/sysctl", "kernel.random.uuid"],
    },
    {"label": "vmstat-disk", "argv": ["/usr/bin/vmstat", "-d", "1", "2"]},
)
SHELL_WRAPPER = {
    "label": "awk",
    "argv": ["bash", "-c", "printf 'a\\n' | awk '{print $1}'"],
}
ENV_SHELL_WRAPPER = {
    "label": "git",
    "argv": [
        "env",
        "REAL_COMPAT_FIXTURES=/tmp/fixtures",
        "bash",
        "/repo/tests/compat/real_compat_workload.sh",
        "git",
    ],
}
DIRECT_BASH = {"label": "bash", "argv": ["/bin/bash", "--version"]}

check(
    "mutant excludes all five scenario-suffixed root programs",
    all(
        legacy_label_matching_form(program) == "shell-or-launcher-wrapped"
        for program in SCENARIO_DIRECTS
    ),
)
check(
    "all five scenario-suffixed direct argv0 programs are eligible",
    all(ANALYZER.named_defect_eligible(program) for program in SCENARIO_DIRECTS),
)
check(
    "scenario-suffixed root executables are reported",
    [ANALYZER.root_executable(program) for program in SCENARIO_DIRECTS]
    == ["mpstat", "pidstat", "sar", "sysctl", "vmstat"],
)
check(
    "genuine bash wrapper remains excluded",
    not ANALYZER.named_defect_eligible(SHELL_WRAPPER),
)
check(
    "env-to-bash workload remains excluded",
    ANALYZER.harness_form(ENV_SHELL_WRAPPER) == "workload-script",
)
check(
    "a test of bash itself remains direct",
    ANALYZER.named_defect_eligible(DIRECT_BASH),
)

if FAILURES:
    print(f"FAIL ({len(FAILURES)} / 6 brackets failed)")
    sys.exit(1)
print("PASS (6 / 6 brackets)")
