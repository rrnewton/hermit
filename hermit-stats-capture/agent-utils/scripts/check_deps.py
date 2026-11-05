#!/usr/bin/env python3
"""Verify every Python console entrypoint starts cleanly without third-party imports.

The YAML-capable tools declare PyYAML as a runtime dependency, while the timeline tool is
stdlib-only. Regardless, `--help`, `--version`, usage errors, and every JSON-only path must work
without PyYAML. This check runs
each entrypoint's dependency-free invocations with the ambient interpreter and fails loudly if any
of them crashes, dumps a Python traceback, or leaks a raw ``ModuleNotFoundError`` — the class of bug
where a third-party dependency is imported at module scope and takes down `--help` on a bare host.

It is intentionally stdlib-only so it runs in the SAME bare runtime environment where the tools are
invoked (no mypy/pytest/PyYAML needed). ``make check-deps`` runs it; ``./setup py`` runs it too.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

# Modules behind every Python console command, including companion commands from a distribution.
ENTRYPOINT_MODULES = [
    "dagrun",
    "dagrun.cpuset_allocator",
    "tick_hub",
    "pr_landing_planner",
    "parallel_experiment_runner",
    "wrkviz",
    "herdr_run",
    "agentctl",
    "agentctl.legacy_cli",
    "agentctl.chat",
    "agentctl.foreign",
    "wrkslots",
]

# Invocations that MUST succeed without importing third-party dependencies.
DEPFREE_ARGS: list[list[str]] = [["--help"], ["--version"], []]

# Force `import yaml` to fail even if PyYAML happens to be installed, so this check reflects the
# bare-host experience on every machine.
_BLOCK_YAML_BOOT = (
    "import sys, runpy\n"
    "sys.modules['yaml'] = None\n"
    "mod = sys.argv[1]\n"
    "sys.argv = [mod] + sys.argv[2:]\n"
    "runpy.run_module(mod, run_name='__main__', alter_sys=True)\n"
)


def _py_dir() -> Path:
    return Path(__file__).resolve().parent.parent / "py"


def _check_one(py_dir: Path, module: str, args: list[str]) -> str | None:
    """Return an error description if this invocation misbehaves, else None."""
    proc = subprocess.run(
        [sys.executable, "-c", _BLOCK_YAML_BOOT, module, *args],
        cwd=py_dir,
        capture_output=True,
        text=True,
        check=False,
    )
    combined = proc.stdout + proc.stderr
    label = f"python3 -m {module} {' '.join(args)}".rstrip()
    if "Traceback (most recent call last)" in combined:
        return f"{label}: dumped a Python traceback with PyYAML absent"
    if "ModuleNotFoundError" in combined:
        return f"{label}: leaked a ModuleNotFoundError with PyYAML absent"
    if proc.returncode != 0:
        return f"{label}: exited {proc.returncode} (expected 0)"
    return None


def main() -> int:
    py_dir = _py_dir()
    failures: list[str] = []
    checked = 0
    for module in ENTRYPOINT_MODULES:
        for args in DEPFREE_ARGS:
            checked += 1
            err = _check_one(py_dir, module, args)
            if err is not None:
                failures.append(err)

    if failures:
        print("check-deps: FAIL — dependency-free entrypoints must never crash:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1

    # Informational: report whether the YAML dependency is present in this environment.
    try:
        import yaml  # noqa: F401

        opt = "PyYAML present (YAML read/write enabled)"
    except ModuleNotFoundError:
        opt = "PyYAML absent (YAML paths will print an actionable install hint; JSON works)"
    print(
        f"check-deps: ok — {checked} dependency-free invocations across "
        f"{len(ENTRYPOINT_MODULES)} entrypoints start cleanly. {opt}."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
