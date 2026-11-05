"""The strict YAML-subset configuration reader, the literate emitter, and format preservation."""

from __future__ import annotations

import json
import math
import os
import subprocess
import sys
from pathlib import Path

import pytest

from wrkslots import cli, sandbox, slotimage, yamlconfig

PACKAGE_ROOT = Path(__file__).resolve().parents[1]
WRKSLOTS = PACKAGE_ROOT / "__main__.py"


# ------------------------------------------------------------------ parser


ACCEPTED = """\
# leading comment
---
schema: 2   # trailing comment
name: "a # not a comment"
single: 'it''s'
plain: hello world
url: http://example.invalid/x#fragment
empty:
tilde: ~
list:
  - a
  - "b c"
  - [1, 2.5, null]
same_indent_list:
- x
- y
maps:
  - k: 1
    v: [a, {}]
  - k: 2
    nested:
      deep: true
flow: {a: 1, b: [x, "y z"], c: null}
empty_flow: []
empty_map: {}
hex: 0x10
octal: 0o17
float: 1.5e3
negative: -3
bool: false
yes_is_a_string: yes
"quoted key": 1
escapes: "tab\\tnewline\\n\\u00e9"
"""


def test_accepted_subset() -> None:
    assert yamlconfig.loads(ACCEPTED) == {
        "schema": 2,
        "name": "a # not a comment",
        "single": "it's",
        "plain": "hello world",
        "url": "http://example.invalid/x#fragment",
        "empty": None,
        "tilde": None,
        "list": ["a", "b c", [1, 2.5, None]],
        "same_indent_list": ["x", "y"],
        "maps": [{"k": 1, "v": ["a", {}]}, {"k": 2, "nested": {"deep": True}}],
        "flow": {"a": 1, "b": ["x", "y z"], "c": None},
        "empty_flow": [],
        "empty_map": {},
        "hex": 16,
        "octal": 15,
        "float": 1500.0,
        "negative": -3,
        "bool": False,
        "yes_is_a_string": "yes",
        "quoted key": 1,
        "escapes": "tab\tnewline\né",
    }
    assert yamlconfig.loads("") is None
    assert yamlconfig.loads("# only a comment\n") is None


@pytest.mark.parametrize(
    "text,line,fragment",
    [
        ("a:\n\tb: 1\n", 2, "tabs"),
        ("a:\n  b: 1\n  \tc: 2\n", 3, "tabs"),
        ("a: &x 1\n", 1, "anchors, aliases, and tags"),
        ("a: *x\n", 1, "anchors, aliases, and tags"),
        ("a: !!str 1\n", 1, "anchors, aliases, and tags"),
        ("&a b: 1\n", 1, "anchors, aliases, and tags"),
        ("a: |\n  text\n", 1, "block scalars"),
        ("a: >-\n  text\n", 1, "block scalars"),
        ("a: one\n  two\n", 2, "multi-line values"),
        ("- a\n  b\n", 2, "multi-line values"),
        ("a: 1\nb: 2\na: 3\n", 3, "duplicate key 'a'"),
        ("a:\n  b: 1\n  b: 2\n", 3, "duplicate key 'b'"),
        ("a: {x: 1, x: 2}\n", 1, "duplicate key 'x'"),
        ("a: [1,\n  2]\n", 1, "multi-line flow"),
        ("%YAML 1.2\na: 1\n", 1, "directives"),
        ("a: 1\n---\nb: 2\n", 2, "document markers"),
        ("? a\n: 1\n", 1, "complex keys"),
        ("a:\n  b: 1\n c: 2\n", 3, "unexpected indentation"),
        ('a: "abc\n', 1, "unterminated"),
        ('a: "\\q"\n', 1, "invalid double-quoted string"),
        ("a: 1\n- b\n", 2, "sequence item cannot follow"),
        ("a: b: c\n", 1, "may not contain ': '"),
        ("  a: 1\n", 1, "column 1"),
        ("a: [1, 2] extra\n", 1, "unexpected text after a flow collection"),
    ],
)
def test_refused_constructs_name_their_line(text: str, line: int, fragment: str) -> None:
    with pytest.raises(yamlconfig.YamlError) as caught:
        yamlconfig.loads(text)
    assert caught.value.line == line, str(caught.value)
    assert fragment in caught.value.reason


def test_json_documents_are_json_and_still_refuse_duplicates() -> None:
    assert yamlconfig.load_document('  {"a": 1, "b": [true, null]}\n') == ({"a": 1, "b": [True, None]}, "json")
    with pytest.raises(yamlconfig.YamlError, match="duplicate key 'a'"):
        yamlconfig.load_document('{"a": 1, "a": 2}')
    with pytest.raises(yamlconfig.YamlError) as caught:
        yamlconfig.load_document('{\n"a": 1,\n}\n')
    assert caught.value.line == 3
    # JSON is also YAML: the YAML reader agrees on flow-style JSON.
    assert yamlconfig.loads('{"a": 1, "b": ["x", 2.5]}') == {"a": 1, "b": ["x", 2.5]}


@pytest.mark.parametrize(
    "value",
    [
        "plain", "with space", "yes", "no", "on", "true", "null", "~", "1", "0x10", "1e3", "-",
        "- item", "a: b", "a #b", "#x", "'q'", '"q"', " lead", "trail ", "", "[x]", "{x}", "é",
        "tab\there", "line\nbreak", "*alias", "&anchor", "!tag", "|", ">", "%", "@", "`",
        ".inf", "a,b", "?", ":",
    ],
)
def test_scalar_text_round_trips(value: str) -> None:
    assert yamlconfig.loads(f"k: {yamlconfig.scalar_text(value)}\n") == {"k": value}
    assert yamlconfig.loads(f"k: [{yamlconfig.scalar_text(value, flow=True)}]\n") == {"k": [value]}


def test_emit_round_trips_every_shape() -> None:
    value: dict[str, object] = {
        "s": "text",
        "i": -7,
        "f": 0.1,
        "big": 1e16,
        "inf": math.inf,
        "b": True,
        "n": None,
        "empty_list": [],
        "empty_map": {},
        "list": ["a", 1, None, False, "yes"],
        "nested": {"x": {"y": [1, 2]}, "z": "a: b"},
        "maps": [{"k": 1, "v": {"w": 2}}, {"only": "one"}, {}],
        "lists": [[1, "a,b"], []],
        "weird key: here": "value",
    }
    text = yamlconfig.emit(value, header="Header.\nSecond line.", comments={("nested", "x"): "About x."}, footer="End.")
    assert yamlconfig.loads(text) == value
    assert text.startswith("# Header.\n# Second line.\n")
    assert "  # About x.\n  x:\n" in text


# ------------------------------------------------------------------ literate config


def _full_payload() -> dict[str, object]:
    payload = cli._config_payload(
        "worktrees/slots", "host-a", "origin", "refs/remotes/origin/main", 60, "tools/live.py",
        liveness_batch_command="tools/batch.py", max_active_slots=4, layout="flat",
        cache_globs=["target"], repo_cache_globs=[("src", ["build", "out/*"])],
        post_provision_hooks=["git submodule update --init"],
        disk_advisory_bytes=30 * cli.GIB, disk_provisioning_floor_bytes=20 * cli.GIB,
        disk_emergency_bytes=10 * cli.GIB, slot_representation="image",
        image={"ceiling_bytes": 64 * cli.GIB, "state_ceiling_bytes": 8 * cli.GIB, "backend": "fuse"},
        sandbox_section={
            **sandbox.default_config_obj(),
            "read_write": ["/var/cache/$USER/x", "~/scratch"],
            "env": {"A_B": "~/x", "EMPTY": ""},
        },
    )
    return payload


def test_every_configuration_key_is_documented() -> None:
    comments = cli.config_comments()
    top = {path[0] for path in comments if len(path) == 1}
    assert top == set(cli.REQUIRED_CONFIG_KEYS | cli.OPTIONAL_CONFIG_KEYS)
    assert {path[1] for path in comments if path[:1] == ("sandbox",) and len(path) == 2} == set(sandbox.SETTING_KEYS)
    assert {path[2] for path in comments if path[:2] == ("sandbox", "limits") and len(path) == 3} == set(
        sandbox.LIMIT_KEYS
    )
    assert {path[1] for path in comments if path[:1] == ("image",) and len(path) == 2} == {
        "ceiling_bytes", "state_ceiling_bytes", "backend"
    } == set(slotimage.IMAGE_DOCS)
    assert all(text.strip() for text in comments.values())


def test_literate_config_round_trips_with_a_comment_above_every_key() -> None:
    payload = _full_payload()
    text = cli.render_config(payload, "yaml")
    assert yamlconfig.loads(text) == payload
    assert text.startswith("# wrkslots project configuration (.wrkslots.yml)")
    assert "wrkslots config convert --to yaml" in text
    lines = text.splitlines()
    for index, line in enumerate(lines):
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or stripped.startswith("- "):
            continue
        key = stripped.split(":", 1)[0]
        parent_is_user_mapping = any(
            lines[back].strip() in ("env:", "repo_cache_globs:") for back in range(max(0, index - 3), index)
        ) and not lines[index - 1].strip().startswith("#")
        if parent_is_user_mapping:
            continue  # env names and repository names are the user's own keys
        assert lines[index - 1].strip().startswith("#"), f"no comment above {key!r}"
    # The historical JSON form round-trips too.
    assert json.loads(cli.render_config(payload, "json")) == payload


def test_minimal_literate_config_lists_the_optional_keys_it_leaves_out() -> None:
    payload = cli._config_payload("worktrees", "h", "origin", "refs/remotes/origin/main", 60, "l.py")
    text = cli.render_config(payload, "yaml")
    assert yamlconfig.loads(text) == payload
    assert "Optional keys not set here" in text and "max_active_slots" in text


# ------------------------------------------------------------------ projects


def _project(tmp_path: Path) -> Path:
    project = tmp_path / "project"
    project.mkdir()
    liveness = project / "liveness.py"
    liveness.write_text("#!/usr/bin/env python3\nraise SystemExit(1)\n", encoding="utf-8")
    liveness.chmod(0o755)
    return project


def _wrkslots(project: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    environment = {**os.environ, "WRKSLOTS_INIT_REPRESENTATION": "worktree"}
    return subprocess.run(
        [sys.executable, str(WRKSLOTS), "--machine", "testhost", *arguments],
        cwd=project,
        capture_output=True,
        text=True,
        timeout=120,
        env=environment,
        check=False,
    )


def _init(project: Path, *extra: str) -> subprocess.CompletedProcess[str]:
    return _wrkslots(project, "init", str(project), "--worktrees-dir", "worktrees", "--liveness-command", "liveness.py", *extra)


def test_new_projects_get_literate_yaml_and_init_is_idempotent(tmp_path: Path) -> None:
    project = _project(tmp_path)
    assert _init(project).returncode == 0
    config = project / ".wrkslots.yml"
    text = config.read_text(encoding="utf-8")
    assert yamlconfig.detect_format(text) == "yaml" and text.startswith("# wrkslots project configuration")
    value = yamlconfig.loads(text)
    assert isinstance(value, dict) and value["sandbox"] == sandbox.default_config_obj()
    before = config.read_bytes()
    rerun = _init(project)
    assert rerun.returncode == 0, rerun.stderr
    assert config.read_bytes() == before and not (project / ".wrkslots.yml.bak").exists()
    assert _wrkslots(project, "status").returncode == 0


def test_init_can_still_write_json(tmp_path: Path) -> None:
    project = _project(tmp_path)
    assert _init(project, "--config-format", "json").returncode == 0
    text = (project / ".wrkslots.yml").read_text(encoding="utf-8")
    assert yamlconfig.detect_format(text) == "json"
    assert json.loads(text)["worktrees_dir"] == "worktrees"


def test_existing_json_projects_stay_json_when_rewritten(tmp_path: Path) -> None:
    project = _project(tmp_path)
    assert _init(project, "--config-format", "json").returncode == 0
    config = project / ".wrkslots.yml"
    value = json.loads(config.read_text(encoding="utf-8"))
    del value["sandbox"]
    original = json.dumps(value, indent=2, sort_keys=True) + "\n"
    config.write_text(original, encoding="utf-8")
    written = _wrkslots(project, "sandbox", "write-defaults")
    assert written.returncode == 0, written.stderr
    rewritten = config.read_text(encoding="utf-8")
    assert yamlconfig.detect_format(rewritten) == "json"
    assert json.loads(rewritten)["sandbox"] == sandbox.default_config_obj()
    assert (project / ".wrkslots.yml.bak").read_text(encoding="utf-8") == original
    assert _wrkslots(project, "image", "set-default", "image").returncode == 0
    assert json.loads(config.read_text(encoding="utf-8"))["slot_representation"] == "image"
    assert (project / ".wrkslots.yml.bak").read_text(encoding="utf-8") == rewritten


def test_yaml_projects_are_regenerated_and_backed_up(tmp_path: Path) -> None:
    project = _project(tmp_path)
    assert _init(project).returncode == 0
    config = project / ".wrkslots.yml"
    hand_edited = config.read_text(encoding="utf-8").replace("tmp_size: 16G", "tmp_size: 4G  # mine") + "# my note\n"
    config.write_text(hand_edited, encoding="utf-8")
    shown = _wrkslots(project, "sandbox", "show-config")
    assert shown.returncode == 0, shown.stderr
    assert json.loads(shown.stdout)["tmp_size"] == "4G"
    assert _wrkslots(project, "image", "set-default", "image").returncode == 0
    regenerated = config.read_text(encoding="utf-8")
    assert "# my note" not in regenerated and "tmp_size: 4G\n" in regenerated
    assert yamlconfig.loads(regenerated)["slot_representation"] == "image"  # type: ignore[index]
    assert (project / ".wrkslots.yml.bak").read_text(encoding="utf-8") == hand_edited


def test_config_convert_both_ways(tmp_path: Path) -> None:
    project = _project(tmp_path)
    assert _init(project, "--config-format", "json").returncode == 0
    config = project / ".wrkslots.yml"
    original = config.read_text(encoding="utf-8")
    value = json.loads(original)
    converted = _wrkslots(project, "config", "convert", "--to", "yaml")
    assert converted.returncode == 0, converted.stderr
    assert "as yaml (was json)" in converted.stdout
    text = config.read_text(encoding="utf-8")
    assert yamlconfig.detect_format(text) == "yaml" and yamlconfig.loads(text) == value
    assert (project / ".wrkslots.yml.bak").read_text(encoding="utf-8") == original
    assert _wrkslots(project, "config", "convert", "--to", "json").returncode == 0
    assert json.loads(config.read_text(encoding="utf-8")) == value
    assert _wrkslots(project, "status").returncode == 0


def test_json_and_yaml_configurations_are_validated_identically(tmp_path: Path) -> None:
    project = _project(tmp_path)
    assert _init(project).returncode == 0
    config = project / ".wrkslots.yml"
    value = yamlconfig.loads(config.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    for fmt in ("yaml", "json"):
        config.write_text(cli.render_config({**value, "surprise": 1}, fmt), encoding="utf-8")
        refused = _wrkslots(project, "status")
        assert refused.returncode != 0 and "unknown surprise" in refused.stderr, (fmt, refused.stderr)
        bad_sandbox = {**value, "sandbox": {"isolation": "namespace"}}
        config.write_text(cli.render_config(bad_sandbox, fmt), encoding="utf-8")
        refused = _wrkslots(project, "status")
        assert refused.returncode != 0 and "renamed userns" in refused.stderr, (fmt, refused.stderr)
    config.write_text("schema: 2\nschema: 2\n", encoding="utf-8")
    refused = _wrkslots(project, "status")
    assert refused.returncode != 0
    assert f"{config}:2: duplicate key 'schema'" in refused.stderr, refused.stderr
    config.write_text(cli.render_config(value, "yaml").replace("tmp_size: 16G", "tmp_size: |"), encoding="utf-8")
    refused = _wrkslots(project, "status")
    assert refused.returncode != 0 and "block scalars" in refused.stderr and f"{config}:" in refused.stderr


def test_deeply_nested_documents_are_refused_not_crashed() -> None:
    deep_flow = "a: " + "[" * 5000 + "]" * 5000 + "\n"
    deep_block = "".join(" " * (2 * depth) + f"k{depth}:\n" for depth in range(1500)) + " " * 3000 + "v: 1\n"
    for text in (deep_flow, deep_block):
        with pytest.raises(yamlconfig.YamlError):
            yamlconfig.loads(text)
