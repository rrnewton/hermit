#!/usr/bin/env python3
"""Reject use of ``typing.Any`` in repository Python source.

Mypy strict mode deliberately permits dynamic library boundaries to be pinned immediately to
``object`` (for example ``raw: object = json.loads(text)``). This companion check ensures nobody
sidesteps that narrowing discipline by importing or referring to ``Any`` in annotations or code.
Comments and documentation may still explain the rule.
"""

from __future__ import annotations

import ast
import sys
from collections.abc import Iterable, Sequence
from pathlib import Path


_IGNORED_DIRS = frozenset(
    {
        ".git",
        ".mypy_cache",
        ".pytest_cache",
        ".venv",
        "__pycache__",
        "build",
        "dist",
        "node_modules",
        "venv",
    }
)


def _python_files(paths: Iterable[Path]) -> list[Path]:
    files: set[Path] = set()
    for path in paths:
        if path.is_dir():
            files.update(
                item
                for item in path.rglob("*.py")
                if item.is_file() and not _IGNORED_DIRS.intersection(item.parts)
            )
        elif path.suffix == ".py" and path.is_file():
            files.add(path)
    return sorted(files)


def _violations(path: Path) -> list[str]:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    result: list[str] = []
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom) and node.module in ("typing", "typing_extensions"):
            for alias in node.names:
                if alias.name == "Any":
                    result.append(f"{path}:{node.lineno}: importing typing.Any is forbidden")
        elif isinstance(node, ast.Name) and node.id == "Any":
            result.append(f"{path}:{node.lineno}: typing.Any reference is forbidden")
        elif isinstance(node, ast.Attribute) and node.attr == "Any":
            result.append(f"{path}:{node.lineno}: .Any reference is forbidden")
    return sorted(set(result))


def main(argv: Sequence[str] | None = None) -> int:
    args = list(argv) if argv is not None else sys.argv[1:]
    roots = [Path(arg) for arg in args] if args else [Path("py"), Path("scripts"), Path("cross")]
    files = _python_files(roots)
    failures = [message for path in files for message in _violations(path)]
    if failures:
        print("check-no-any: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print(f"check-no-any: ok — {len(files)} Python files contain no typing.Any")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
