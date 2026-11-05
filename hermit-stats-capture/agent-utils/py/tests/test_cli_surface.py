"""Keep runnable Python modules aligned with declared package entry points."""

from __future__ import annotations

import ast
import re
from pathlib import Path


PY_ROOT = Path(__file__).resolve().parents[1]
ENTRY_POINT = re.compile(
    r'^[-\w]+\s*=\s*"(?P<module>[a-zA-Z_][\w.]*)\:(?P<function>[a-zA-Z_]\w*)"$',
    re.MULTILINE,
)

# These helpers are exec targets for trusted parent processes and require
# inherited private descriptors. Publishing them as user-facing console scripts
# would expose a nonsensical and unsafe interface, but their exact callable
# surface remains declared here so a new CLI-shaped module cannot evade review.
PRIVATE_ENTRY_POINTS = frozenset({
    ("agentctl._command_anchor", "main"),
    ("agentctl._command_supervisor", "main"),
    ("agentctl._event_command_supervisor", "main"),
    ("agentctl._launch_gate", "main"),
})


def test_public_main_functions_are_declared_entry_points() -> None:
    """Reject CLI-shaped modules that package installers cannot discover."""
    declared: set[tuple[str, str]] = set()
    manifests = sorted(PY_ROOT.glob("*/pyproject.toml"))
    assert manifests
    for manifest in manifests:
        for match in ENTRY_POINT.finditer(manifest.read_text(encoding="utf-8")):
            declared.add((match.group("module"), match.group("function")))

    discovered: set[tuple[str, str]] = set()
    package_dirs = sorted(manifest.parent for manifest in manifests)
    for package_dir in package_dirs:
        for source in sorted(package_dir.glob("*.py")):
            module = ".".join(source.relative_to(PY_ROOT).with_suffix("").parts)
            tree = ast.parse(source.read_text(encoding="utf-8"), filename=str(source))
            for node in tree.body:
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == "main":
                    discovered.add((module, node.name))

    assert declared.isdisjoint(PRIVATE_ENTRY_POINTS)
    assert all(module.rsplit(".", 1)[-1].startswith("_")
               for module, _ in PRIVATE_ENTRY_POINTS)
    assert discovered == declared | PRIVATE_ENTRY_POINTS
