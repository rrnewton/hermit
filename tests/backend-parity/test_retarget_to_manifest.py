#!/usr/bin/env python3
"""Refuse legacy backend-parity rows as current DBT or KVM qualification."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from retarget_to_manifest import build_plan, parse_matrix_row, render_test_block  # noqa: E402


FAILURES: list[str] = []


def check(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  \033[32mok\033[0m    {label}")
    else:
        FAILURES.append(label)
        print(f"  \033[31mFAIL\033[0m  {label}" + (f" -- {detail}" if detail else ""))


def plan_for(row: str):
    parsed = parse_matrix_row(row)
    return build_plan(parsed, f"tests/c/{parsed.name}.c", None)


print("case LEGACY-L2 — old detlog/guest tokens do not enable a backend")
plan = plan_for("probe\tpass\tpass\tpass\t-\t-\tdetlog\tdetlog\tguest\t-\t-")
check("only ptrace is enabled", plan.enabled == ["ptrace"], repr(plan.enabled))
check("DBT requires a fresh verdict from the protected evidence path",
      "current protected canonical evidence path" in plan.disabled["dbt"],
      plan.disabled["dbt"])
check("KVM remains unqualified",
      "KVM remains unqualified" in plan.disabled["kvm"],
      plan.disabled["kvm"])
rendered = render_test_block(plan)
check("rendered TOML enables only ptrace",
      'backends_enabled = ["ptrace"]' in rendered, rendered)

print("case ADVERSARIAL-KVM — even a legacy detlog token cannot promote KVM")
plan = plan_for("probe\tpass\tpass\tpass\t-\t-\tdetlog\tguest\tdetlog\t-\t-")
check("KVM detlog remains disabled", "kvm" in plan.disabled, repr(plan.disabled))
check("KVM is absent from enabled", "kvm" not in plan.enabled, repr(plan.enabled))

print("case L1-ONLY — a row with no L2 evidence remains ptrace-only")
plan = plan_for("probe\tpass\tpass\tpass\t-\t-")
check("six-column row enables only ptrace", plan.enabled == ["ptrace"], repr(plan.enabled))
check("DBT L1 reason requires a fresh non-vacuous L2 verdict",
      "protected canonical evidence path" in plan.disabled["dbt"]
      and "no fresh typed, non-vacuous L2 verdict" in plan.disabled["dbt"])
check("KVM L1 reason remains explicit", "L2 --verify witness was not recorded" in plan.disabled["kvm"])

if FAILURES:
    print(f"\n{len(FAILURES)} failure(s):")
    for failure in FAILURES:
        print(f"  - {failure}")
    raise SystemExit(1)
print("\nall retarget refusal brackets passed")
