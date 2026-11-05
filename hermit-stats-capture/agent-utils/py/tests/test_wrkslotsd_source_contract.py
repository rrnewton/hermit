"""Cross-language source contracts for the staged wrkslots observer."""

from __future__ import annotations

import ast
import re
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PYTHON_AUTHORITY = REPO_ROOT / "py" / "wrkslots" / "cli.py"
RUST_REPLAY = REPO_ROOT / "rs" / "wrkslotsd" / "src" / "replay.rs"
STATE_EVENT_KINDS = {
    "active-state-recorded",
    "archive-state-recorded",
    "operation-completed",
    "operation-progress-recorded",
    "reclaim-started",
    "recovery-started",
    "retirement-attempted",
    "slot-held",
    "slot-hold-released",
    "state-imported",
}
EVENT_WRITER_NAMES = {"event_writer", "writer"}


def _mentions_event_writer(node: ast.AST | None) -> bool:
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        try:
            node = ast.parse(node.value, mode="eval")
        except SyntaxError:
            return False
    return node is not None and any(
        isinstance(item, ast.Name) and item.id == "_EventWriter"
        for item in ast.walk(node)
    )


def _constructs_event_writer(node: ast.AST) -> bool:
    return any(
        isinstance(item, ast.Call)
        and isinstance(item.func, ast.Name)
        and item.func.id in {"_event_writer", "_EventWriter"}
        for item in ast.walk(node)
    )


def _bound_names(node: ast.AST) -> set[str]:
    return {
        item.id
        for item in ast.walk(node)
        if isinstance(item, ast.Name) and isinstance(item.ctx, ast.Store)
    }


def _python_emitted_event_kinds() -> set[str]:
    tree = ast.parse(PYTHON_AUTHORITY.read_text(encoding="utf-8"))
    writer_bindings: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            arguments = [*node.args.posonlyargs, *node.args.args, *node.args.kwonlyargs]
            writer_bindings.update(
                argument.arg
                for argument in arguments
                if _mentions_event_writer(argument.annotation)
            )
        elif isinstance(node, ast.Assign) and _constructs_event_writer(node.value):
            writer_bindings.update(
                name for target in node.targets for name in _bound_names(target)
            )
        elif isinstance(node, ast.AnnAssign) and _mentions_event_writer(node.annotation):
            if isinstance(node.target, ast.Name):
                writer_bindings.add(node.target.id)
    assert writer_bindings <= EVENT_WRITER_NAMES, (
        "event-writer bindings must use an audited name: "
        f"{sorted(writer_bindings - EVENT_WRITER_NAMES)}"
    )

    kinds: set[str] = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        candidate: ast.expr | None = None
        if isinstance(node.func, ast.Name) and node.func.id == "_write_event_file":
            if len(node.args) >= 3:
                candidate = node.args[2]
            else:
                candidate = next(
                    (item.value for item in node.keywords if item.arg == "kind"), None
                )
        elif (
            isinstance(node.func, ast.Attribute)
            and node.func.attr == "append"
            and isinstance(node.func.value, ast.Name)
            and node.func.value.id in EVENT_WRITER_NAMES
        ):
            candidate = node.args[0] if node.args else None
        if candidate is None:
            continue
        assert isinstance(candidate, ast.Constant) and isinstance(candidate.value, str), (
            "event writers must use literal kinds so Rust/Python parity is auditable: "
            f"line {node.lineno}"
        )
        kinds.add(candidate.value)
    return kinds


def _rust_non_state_event_kinds() -> set[str]:
    source = RUST_REPLAY.read_text(encoding="utf-8")
    declaration = re.search(
        r"const NON_STATE_EVENT_KINDS: &\[&str\] = &\[(?P<body>.*?)\];",
        source,
        flags=re.DOTALL,
    )
    assert declaration is not None
    return set(re.findall(r'"([^"]+)"', declaration.group("body")))


def _rust_state_event_kinds() -> set[str]:
    source = RUST_REPLAY.read_text(encoding="utf-8")
    dispatch = re.search(
        r"fn apply_event\(.*?match event\.kind\.as_str\(\) \{"
        r"(?P<body>.*?)kind if NON_STATE_EVENT_KINDS",
        source,
        flags=re.DOTALL,
    )
    assert dispatch is not None
    return set(
        re.findall(
            r'^\s*"([^"]+)"\s*=>\s*apply_[a-z_]+\(event, state\),$',
            dispatch.group("body"),
            flags=re.MULTILINE,
        )
    )


def test_rust_replay_knows_every_python_emitted_event_kind() -> None:
    """A new Python event kind cannot silently make every Rust rebuild fail."""

    emitted = _python_emitted_event_kinds()
    assert STATE_EVENT_KINDS <= emitted
    assert _rust_state_event_kinds() == STATE_EVENT_KINDS
    assert emitted - STATE_EVENT_KINDS == _rust_non_state_event_kinds()
