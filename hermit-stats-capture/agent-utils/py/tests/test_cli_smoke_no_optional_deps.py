"""Regression guard: every console entrypoint must survive `--help` / `--version` / no-args with
ZERO optional dependencies installed, and must turn a genuinely-needed-but-missing optional
dependency into an ACTIONABLE message instead of a Python traceback.

Motivation: PyYAML is an OPTIONAL dependency of this tree (only the YAML read/write paths need it),
but it used to be imported at module scope, so `dagrun -h` crashed with a bare
``ModuleNotFoundError: No module named 'yaml'`` on any host without PyYAML. The owner's interim
policy is to run these Python entrypoints directly, so `-h` crashing is unacceptable.

Each case runs in a SUBPROCESS with ``sys.modules['yaml'] = None`` injected BEFORE the tool is
imported, which forces ``import yaml`` to raise ``ModuleNotFoundError`` regardless of whether PyYAML
is actually installed on the test host (so the guard holds in CI, where PyYAML *is* present).
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

# Bootstrap run under `python3 -c`: block PyYAML, then run the requested module as __main__ with the
# remaining argv. Keeps the "no optional deps" condition identical on every host.
_BLOCK_YAML_BOOT = (
    "import sys, runpy\n"
    "sys.modules['yaml'] = None\n"  # any `import yaml` now raises ModuleNotFoundError
    "mod = sys.argv[1]\n"
    "sys.argv = [mod] + sys.argv[2:]\n"
    "runpy.run_module(mod, run_name='__main__', alter_sys=True)\n"
)

# The primary ``python -m`` entrypoint for each independently packaged tool.
ENTRYPOINT_MODULES = ["dagrun", "tick_hub", "pr_landing_planner"]

# The dependency-free invocations the owner explicitly asked to always work.
DEPFREE_FLAGS = ["-h", "--help", "--version", ""]


def _run_blocked(module: str, args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-c", _BLOCK_YAML_BOOT, module, *args],
        capture_output=True,
        text=True,
        check=False,
    )


@pytest.mark.parametrize("module", ENTRYPOINT_MODULES)
@pytest.mark.parametrize("flag", DEPFREE_FLAGS)
def test_depfree_invocations_never_touch_optional_deps(module: str, flag: str) -> None:
    """`-h` / `--help` / `--version` / no-args must exit 0 with no traceback, even with no PyYAML."""
    args = [flag] if flag else []
    result = _run_blocked(module, args)
    combined = result.stdout + result.stderr
    assert "Traceback (most recent call last)" not in combined, (
        f"{module} {flag!r} dumped a traceback with PyYAML absent:\n{combined}"
    )
    assert "ModuleNotFoundError" not in combined, (
        f"{module} {flag!r} leaked a ModuleNotFoundError with PyYAML absent:\n{combined}"
    )
    assert result.returncode == 0, (
        f"{module} {flag!r} exited {result.returncode} (expected 0):\n{combined}"
    )


def test_cpuset_companion_help_starts_without_yaml() -> None:
    """The ``dagrun`` distribution's companion console command is independently startable."""
    result = _run_blocked("dagrun.cpuset_allocator", ["--help"])
    combined = result.stdout + result.stderr
    assert result.returncode == 0, combined
    assert "Traceback (most recent call last)" not in combined
    assert "ModuleNotFoundError" not in combined


# (module, argv) pairs that genuinely reach a YAML read/write path; each must degrade to a clean,
# actionable message (exit 2, no traceback) rather than crashing when PyYAML is unavailable.
YAML_PATH_CASES = [
    ("dagrun", ["yaml", "--dag", "{json_dag}"]),  # dag_to_yaml
    ("tick_hub", ["yaml", "--config", "{json_cfg}"]),  # config_to_yaml
    ("pr_landing_planner", ["plan", "--fixture", "{yaml_fixture}"]),  # load_fixture_text (YAML)
]


@pytest.mark.parametrize("module,argv_template", YAML_PATH_CASES)
def test_missing_yaml_is_actionable_not_a_traceback(
    module: str, argv_template: list[str], tmp_path: Path
) -> None:
    """A real YAML operation without PyYAML prints an install hint and exits 2 (no traceback)."""
    base = tmp_path
    json_dag = base / "dag.json"
    json_dag.write_text('{"description":"t","steps":[{"group":"b","job":"a","cmd":"true"}]}\n')
    json_cfg = base / "cfg.json"
    json_cfg.write_text('{"reminders":[]}\n')
    yaml_fixture = base / "fixture.yaml"
    yaml_fixture.write_text("prs: []\n")

    argv = [
        a.format(json_dag=json_dag, json_cfg=json_cfg, yaml_fixture=yaml_fixture)
        for a in argv_template
    ]
    result = _run_blocked(module, argv)
    combined = result.stdout + result.stderr
    assert "Traceback (most recent call last)" not in combined, (
        f"{module} {argv} dumped a traceback instead of an actionable message:\n{combined}"
    )
    assert result.returncode == 2, (
        f"{module} {argv} exited {result.returncode} (expected 2):\n{combined}"
    )
    # The message must name PyYAML and how to install it.
    assert "PyYAML" in combined and "pip install" in combined, (
        f"{module} {argv} message is not actionable:\n{combined}"
    )
