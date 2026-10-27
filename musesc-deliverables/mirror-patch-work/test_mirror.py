"""Direct publication transport tests with real temporary Git repositories.

The default fast suite uses explicit Rust/checker/compiler fixture doubles.
The separate --real-finalized-provider JSON integration invocation uses freshly
source-bound native helpers, the actual canonical reader and combined writer.
Its compiler transport reuses those executables without creating a build target
inside temporary STATE. Neither suite executes a Hermit guest.
"""
from __future__ import annotations

import ast
import contextlib
import copy
import errno
import fcntl
import importlib.util
import gc
import io
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import tracemalloc
import threading
import time
import unittest
import weakref
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest import mock

MODULE = Path(__file__).resolve().parents[1] / "mirror.py"
SPEC = importlib.util.spec_from_file_location("series_mirror", MODULE)
assert SPEC is not None and SPEC.loader is not None
mirror = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mirror)
series = mirror.series
HELP_FIXTURE = (
    "if sys.argv[1:] == ['--help']:\n"
    " assert 'DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD' not in os.environ\n"
    " assert 'DEV_HERMIT_TEST_LEDGER_ROOT' not in os.environ\n"
    " raise SystemExit(0)\n"
)


#: rustc, cc, cargo and rust-script as the rust tool cache runs them, for
#: DirectLedgerFixture.enable_rust_tool_cache(). Each call is logged to
#: calls.jsonl beside it; an invocation of the direct path goes on to the
#: direct-path double of the same name in ../bin, which checks its argv.
CACHE_TOOLCHAIN_FIXTURE = f"""#!{sys.executable}
import json, os, pathlib, sys, tomllib
HERE = pathlib.Path(__file__).resolve().parent
name, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
with open(HERE / "calls.jsonl", "a") as log:
    log.write(json.dumps([name, *args]) + "\\n")
def option(flag):
    return args[args.index(flag) + 1]
if name == "rustc":
    print("rustc 1.90.0-nightly (fixture)\\nhost: x86_64-unknown-linux-gnu")
elif name == "cc":
    print("cc (fixture) 12.0")
elif name == "rust-script" and args[:2] == ["--package", "--pkg-path"]:
    package, script = pathlib.Path(args[2]), pathlib.Path(args[3])
    package.mkdir(parents=True)
    (package / "Cargo.toml").write_text(
        f'[[bin]]\\nname = "{{script.stem}}_fixture"\\npath = "{{script}}"\\n')
elif name == "cargo" and args == ["-vV"]:
    print("cargo 1.90.0-nightly (fixture)")
elif name == "cargo" and args[0] == "generate-lockfile":
    pathlib.Path(option("--manifest-path")).with_name("Cargo.lock").write_text("# fixture lock\\n")
elif name == "cargo" and args[0] == "fetch":
    assert "--locked" in args, args
elif name == "cargo" and args[0] == "build":
    assert "--locked" in args and "--offline" in args, args
    if (HERE / "fail-build").exists():
        raise SystemExit("error: fixture build failure")
    if "-p" in args:
        binary, profile, source = option("--bin"), "debug", "#!/bin/sh\\nexit 0\\n"
    else:
        target = tomllib.loads(pathlib.Path(option("--manifest-path")).read_text())["bin"][0]
        binary, profile = target["name"], "release"
        source = pathlib.Path(target["path"]).read_text()
    output = pathlib.Path(option("--target-dir")) / profile / binary
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(source)
    output.chmod(0o755)
    print(json.dumps({{"reason": "compiler-artifact", "target": {{"name": binary}},
                      "executable": str(output)}}))
else:
    direct = HERE.parent / "bin" / name
    os.execv(direct, [str(direct), *args])
"""


def bounded_fixture_command(command: list[str]) -> list[str]:
    """Install the existing checker/query doubles inside the real bounded worker.

    Production re-exec cannot carry an in-process runpy patch across exec. The
    fixture keeps that same bootstrap inside the actual cleanup domain instead;
    mirror still verifies its deadline/cgroup context before acquiring a lock.
    The original subprocess harness has a 30-second ceiling.
    """
    deadline = time.monotonic_ns() + 30_000_000_000
    return [str(mirror.TOOL_ROOT / "scripts/hermit-box-run"), "--passthrough",
            "--inherited-deadline-ns", str(deadline), "--diag-fd", "2", "--",
            *command, "--lifecycle-deadline-ns", str(deadline), "--bounded-worker"]


def row(identifier: str, outcome: str = "passed") -> dict:
    return {
        "schema": "stress-series/v1", "event_id": identifier,
        "event_type": "series.observation", "emitted_at": "2026-09-21T12:00:00Z",
        "team": "hermit", "host": "fixture", "run_id": identifier,
        "producer": "fixture", "series": {
            "cell": "c-programs/hello/verify/ptrace", "tree": "a" * 40,
            "run_index": 0, "outcome": outcome,
        },
    }


def git(repo: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def git_bytes(repo: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(repo), *args])

def _apply_case(row: dict, case: dict) -> dict:
    """Apply a release-payload-cases.json case: drop the pair and every absent
    key, then set exactly the case's fields."""
    for key in ("release_builder", "e2e_payload", *case.get("absent", ())):
        row.pop(key, None)
    row.update(case["fields"])
    return row



def stored_history(repo: Path, commit: str = "main") -> bytes | None:
    """The scorecard history the ledger stores at COMMIT, in either layout."""
    return mirror.scorecard_cells.read_commit(repo, commit)


def history_entries(repo: Path, commit: str = "main") -> str:
    """Every tree entry storing the scorecard history at COMMIT: mode, blob name and path."""
    return git(repo, "ls-tree", "-r", "--full-tree", commit, "--",
               mirror.scorecard_cells.LOGICAL_PATH, mirror.scorecard_cells.PARTS_DIRECTORY)


class DirectLedgerFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="direct-ledger-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.remote = self.root / "remote.git"
        config = self.root / "gitconfig"
        config.write_text(
            '[user]\n name = Fixture\n email = fixture@example.invalid\n'
            '[init]\n defaultBranch = main\n'
            f'[url "{self.remote}"]\n insteadOf = {mirror.REMOTE_URL}\n'
        )
        self.enterContext(mock.patch.dict(os.environ, {
            "GIT_CONFIG_GLOBAL": str(config), "GIT_CONFIG_NOSYSTEM": "1",
            "WITH_PROXY": "/usr/bin/env", "PYTHONDONTWRITEBYTECODE": "1",
        }))
        for key in (series.LEDGER_ROOT_ENV, "DEV_HERMIT_TOOL_AUTHORITY", "CARGO_TARGET_DIR"):
            self.enterContext(mock.patch.dict(os.environ))
            os.environ.pop(key, None)
        self.enterContext(mock.patch.object(
            series, "_run_shared_checker",
            return_value=subprocess.CompletedProcess(["schema-fixture"], 0, "", ""),
        ))
        self.enterContext(mock.patch.object(mirror.publisher, "_series_store", return_value=series))
        subprocess.run(["git", "init", "--bare", "-q", str(self.remote)], check=True)
        seed = self.root / "seed"
        subprocess.run(["git", "init", "-q", str(seed)], check=True)
        (seed / "series/hermit/fixture").mkdir(parents=True)
        (seed / "series/hermit/fixture/2026-09.jsonl").write_text("")
        (seed / "scorecard").mkdir()
        (seed / "scorecard/cells.json").write_text('{"legacy":"preserve"}\n')
        (seed / "scorecard/SCORECARD.md").write_text("legacy scorecard\n")
        git(seed, "add", "series", "scorecard")
        git(seed, "commit", "-qm", "seed existing ledger")
        git(seed, "remote", "add", "origin", str(self.remote))
        git(seed, "push", "-q", "origin", "main")
        self.parent = self.root / "parent"
        subprocess.run(["git", "init", "-q", str(self.parent)], check=True)
        (self.parent / ".gitmodules").write_text(
            '[submodule "hermit_test_ledger"]\n path = hermit_test_ledger\n'
            f' url = {mirror.REMOTE_URL}\n'
        )
        (self.parent / ".gitignore").write_text("ignored/\nhermit_test_ledger/\n")
        (self.parent / "series/hermit/fixture").mkdir(parents=True)
        (self.parent / "series/hermit/fixture/2026-09.jsonl").write_text(json.dumps(row("historical-parent")) + "\n")
        git(self.parent, "add", ".gitmodules", ".gitignore", "series")
        git(self.parent, "commit", "-qm", "parent history before migration")
        self.parent_commit = git(self.parent, "rev-parse", "HEAD")
        self.ledger = self.parent / "hermit_test_ledger"
        subprocess.run(["git", "clone", "-q", mirror.REMOTE_URL, str(self.ledger)], check=True)
        self.source = self.root / "hermit"
        subprocess.run(["git", "init", "-q", str(self.source)], check=True)
        (self.source / "measured-source").write_text("measured checkout\n")
        (self.source / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "nightly-2025-01-01"\n')
        git(self.source, "add", "measured-source", "rust-toolchain.toml")
        git(self.source, "commit", "-qm", "measured source")
        self.target = git(self.source, "rev-parse", "HEAD")
        self.tool = self.root / "tool-hermit"
        subprocess.run(["git", "init", "-q", str(self.tool)], check=True)
        tool = self.tool / "ci/compat-envelope/scorecard.rs"
        tool.parent.mkdir(parents=True)
        (self.tool / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "nightly-2026-07-12"\n')
        compiler = self.root / "bin/rust-script"
        compiler.parent.mkdir()
        compiler.write_text(
            "#!/usr/bin/python3\nimport os, sys\n"
            "assert sys.argv[1:3] == ['--force', '--toolchain']\n"
            "assert sys.argv[3] == 'nightly-2026-07-12-x86_64-unknown-linux-gnu'\n"
            "assert os.environ['RUSTUP_TOOLCHAIN'] == sys.argv[3]\n"
            "assert os.environ['RUSTUP_AUTO_INSTALL'] == '0'\n"
            "os.execv(sys.argv[4], sys.argv[4:])\n"
        )
        compiler.chmod(0o755)
        cargo = compiler.with_name("cargo")
        cargo.write_text(
            "#!/usr/bin/python3\nimport os, sys\n"
            f"assert sys.argv[1:] == {['run', '--quiet', '-p', 'hermit-manifest-plan', '--manifest-path', str(self.tool / 'Cargo.toml'), '--target-dir', str(self.parent / 'target'), '--', '--help']!r}\n"
            "assert os.environ['RUSTUP_TOOLCHAIN'] == 'nightly-2026-07-12-x86_64-unknown-linux-gnu'\n"
            "assert os.environ['RUSTUP_AUTO_INSTALL'] == '0'\n"
            "assert 'DEV_HERMIT_TEST_LEDGER_ROOT' not in os.environ\n"
            "assert 'DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD' not in os.environ\n"
        )
        cargo.chmod(0o755)
        self.enterContext(mock.patch.dict(os.environ, {
            "PATH": str(compiler.parent) + os.pathsep + os.environ["PATH"],
            "RUSTUP_TOOLCHAIN": "nightly-2025-01-01",
            "RUSTUP_AUTO_INSTALL": "1",
        }))
        # A Python fixture child exercises inherited authority and output routing.
        # Like scorecard.rs, it also reads the history at HEAD, whichever layout
        # the ledger stores it in.
        tool.write_text("#!/usr/bin/python3\nimport os, pathlib, subprocess, sys\n"
                        + HELP_FIXTURE +
                        "fd=int(os.environ['DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD'])\n"
                        "os.fstat(fd)\n"
                        "p=pathlib.Path(os.environ['DEV_HERMIT_TEST_LEDGER_ROOT'])/'scorecard'\n"
                        "assert (p/'cells.json').read_text() == '{\"legacy\":\"preserve\"}\\n'\n"
                        "assert subprocess.check_output(['git','-C',str(p.parent),'show','HEAD:scorecard/cells.json'])"
                        " == b'{\"legacy\":\"preserve\"}\\n'\n"
                        "(p/'SCORECARD.md').write_text('derived from existing history\\n')\n")
        tool.chmod(0o755)
        git(self.tool, "add", "ci", "rust-toolchain.toml")
        git(self.tool, "commit", "-qm", "fixture scorecard tool")
        # These tests exercise the direct path, which a publication takes
        # whenever the rust tool cache (rust_tool_cache.py) cannot serve it;
        # enable_rust_tool_cache() lets a test use the cache instead.
        self.direct_path = mock.patch.object(
            mirror.rust_tool_cache, "prepare",
            side_effect=mirror.rust_tool_cache.Unavailable("fixture: direct path"))
        self.direct_path.start()
        self.addCleanup(self.direct_path.stop)
        # These fixtures render many times within seconds; the tests of the
        # regeneration interval set it themselves.
        self.enterContext(mock.patch.object(mirror, "SCORECARD_REGENERATION_INTERVAL_SECONDS", 0))

    def append(self, identifier: str, outcome: str = "passed") -> Path:
        path = series.spool_dir(self.parent) / f"{identifier}.jsonl"
        series.append_cell_rows(self.parent, path, [row(identifier, outcome)])
        return path

    def publish(self, **kwargs):
        return mirror.mirror(
            self.parent, self.source, self.target, mirror.REMOTE_URL,
            net_wrapper="/usr/bin/env", tool_checkout=self.tool, **kwargs,
        )

    def remote_rows(self):
        payload = git(self.remote, "show", "main:series/hermit/fixture/2026-09.jsonl")
        return [json.loads(line) for line in payload.splitlines()]

    def published_projection(self) -> dict[str, str]:
        """The blob names main publishes for latest.json, SCORECARD.md and the stored history."""
        projection = {path: git(self.remote, "rev-parse", "main:" + path)
                      for path in ("latest.json", "scorecard/SCORECARD.md")}
        projection[mirror.scorecard_cells.LOGICAL_PATH] = history_entries(self.remote)
        self.assertTrue(projection[mirror.scorecard_cells.LOGICAL_PATH], "main stores no history")
        return projection

    def enable_rust_tool_cache(self) -> Path:
        """Serve this test's compiles from the rust tool cache, built by a fake toolchain.

        The scorecard double logs whether it ran as a stored program or
        through the direct path's rust-script to renders.log. Returns the
        fake toolchain's directory.
        """
        self.direct_path.stop()
        fakes = self.root / "cache-bin"
        fakes.mkdir()
        for name in ("rustc", "cc", "cargo", "rust-script"):
            (fakes / name).write_text(CACHE_TOOLCHAIN_FIXTURE)
            (fakes / name).chmod(0o755)
        cargo_home = self.root / "cargo-home"
        cargo_home.mkdir()
        # setUp's patch.dict restores the whole environment afterwards.
        os.environ["PATH"] = str(fakes) + os.pathsep + os.environ["PATH"]
        os.environ["CARGO_HOME"] = str(cargo_home)
        scorecard = self.tool / "ci/compat-envelope/scorecard.rs"
        shebang, body = scorecard.read_text().split("\n", 1)
        scorecard.write_text(
            shebang + "\nimport os, pathlib, sys\n"
            "stored = '/rust-tool-cache/' in sys.argv[0]\n"
            "if stored:\n"
            " assert os.path.isfile(os.environ['HERMIT_MANIFEST_PLAN_BIN'])\n"
            f" assert os.environ['RUST_SCRIPT_PATH'] == {str(scorecard.resolve())!r}\n"
            f"with open({str(self.root / 'renders.log')!r}, 'a') as log:\n"
            " log.write(('stored ' if stored else 'direct ') + sys.argv[1] + '\\n')\n"
            + body)
        (self.tool / "Cargo.toml").write_text('[workspace]\nmembers = ["ci/manifest-plan"]\n')
        (self.tool / "Cargo.lock").write_text("# workspace lock fixture\n")
        git(self.tool, "add", "Cargo.toml", "Cargo.lock", "ci")
        git(self.tool, "commit", "-qm", "workspace for the rust tool cache")
        return fakes

    def renders(self) -> list[str]:
        log = self.root / "renders.log"
        return log.read_text().splitlines() if log.exists() else []

    def cache_builds(self, fakes: Path) -> list[list[str]]:
        log = fakes / "calls.jsonl"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return [call for call in calls if call[:2] == ["cargo", "build"]]

    def scorecard_slot_free(self) -> bool:
        lock = self.parent / mirror.rust_tool_cache.CACHE_RELATIVE / "scorecard/0/lock"
        with open(lock, "rb") as handle:
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            return True

    def test_publication_renders_with_the_programs_the_rust_tool_cache_stored(self):
        fakes = self.enable_rust_tool_cache()
        self.append("cached-one")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish()
        self.assertEqual("published", report["state"], stderr.getvalue())
        self.assertEqual([row("cached-one")], self.remote_rows())
        self.assertEqual(git(self.tool, "rev-parse", "HEAD"), report["scorecard"]["tool_hermit_commit"])
        self.assertEqual("derived from existing history",
                         git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        # Built once: the helper and the scorecard.
        self.assertEqual(2, len(self.cache_builds(fakes)), stderr.getvalue())
        self.assertTrue(self.renders())
        self.assertTrue(all(line.startswith("stored ") for line in self.renders()), self.renders())
        self.assertIn("scorecard renders with stored programs", stderr.getvalue())
        self.assertNotIn("tool cache unavailable", stderr.getvalue())
        self.assertTrue(self.scorecard_slot_free(), "the publication still holds its slot")
        # A later publication of the same tool commit builds nothing.
        self.append("cached-two")
        with contextlib.redirect_stderr(io.StringIO()):
            again = self.publish()
        self.assertEqual("published", again["state"], again)
        self.assertEqual([row("cached-one"), row("cached-two")], self.remote_rows())
        self.assertEqual(2, len(self.cache_builds(fakes)))
        self.assertTrue(all(line.startswith("stored ") for line in self.renders()), self.renders())
        self.assertTrue(self.scorecard_slot_free())

    def test_publication_takes_the_direct_path_when_the_rust_tool_cache_fails(self):
        fakes = self.enable_rust_tool_cache()
        (fakes / "fail-build").write_text("")
        self.append("cache-failed")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish()
        self.assertEqual("published", report["state"], stderr.getvalue())
        self.assertEqual([row("cache-failed")], self.remote_rows())
        self.assertIn("scorecard tool cache unavailable", stderr.getvalue())
        self.assertIn("fixture build failure", stderr.getvalue())
        self.assertNotIn("renders with stored programs", stderr.getvalue())
        self.assertTrue(self.renders())
        self.assertTrue(all(line.startswith("direct ") for line in self.renders()), self.renders())
        self.assertIn("direct --help", self.renders())
        self.assertTrue(self.scorecard_slot_free(), "a failed build still holds its slot")
        store = self.parent / mirror.rust_tool_cache.CACHE_RELATIVE / "scorecard/0/store"
        self.assertEqual([], [entry.name for entry in store.iterdir()] if store.exists() else [])

    def test_direct_publication_preserves_parent_and_legacy_history(self):
        self.append("run-one")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertEqual(mirror.REMOTE_URL, report["repository"])
        git(self.remote, "merge-base", "--is-ancestor", report["series_commit"], "main")
        self.assertEqual([row("run-one")], self.remote_rows())
        self.assertEqual(self.parent_commit, git(self.parent, "rev-parse", "HEAD"))
        self.assertEqual("", git(self.source, "status", "--porcelain"))
        self.assertEqual(git(self.tool, "rev-parse", "HEAD"), report["scorecard"]["tool_hermit_commit"])
        self.assertNotEqual(self.target, report["scorecard"]["tool_hermit_commit"])
        self.assertFalse((self.source / "target").exists())
        # The first publication stores the preserved history in parts.
        self.assertEqual(b'{"legacy":"preserve"}\n', stored_history(self.remote))
        self.assertEqual(["scorecard/cells/manifest.json"],
                         git(self.remote, "ls-tree", "-r", "--name-only", "main", "--",
                             "scorecard/cells.json", "scorecard/cells").splitlines())
        self.assertEqual([], list(series.spool_dir(self.parent).glob("*.jsonl")))
        self.assertTrue(list((series.spool_dir(self.parent) / "published").glob("*.jsonl")))
        again = self.publish()
        self.assertEqual(b'{"legacy":"preserve"}\n', stored_history(self.remote))
        self.assertEqual([row("run-one")], self.remote_rows())
        self.assertEqual(report["series_commit"], report["source_commit"])
        self.assertEqual(0, again["raw_publication"]["published"])
        linked = [line for line in git(self.ledger, "worktree", "list", "--porcelain").splitlines()
                  if line.startswith("worktree ")]
        self.assertEqual(2, len(linked), "repeated publication reuses one linked writer")

    def test_read_only_tool_uses_mutable_state_target_and_its_own_compiler(self):
        self.append("immutable-tool")
        source = self.tool / "ci/compat-envelope/scorecard.rs"
        source.write_text(source.read_text() +
            "target=pathlib.Path(os.environ['CARGO_TARGET_DIR'])\n"
            f"assert target == pathlib.Path({str(self.parent / 'target')!r})\n"
            "target.mkdir(exist_ok=True)\n"
            "(target/'fixture-build-output').write_text('mutable output\\n')\n")
        git(self.tool, "add", "ci")
        git(self.tool, "commit", "-qm", "read-only tool build transport fixture")
        self.tool.chmod(0o555)
        self.addCleanup(self.tool.chmod, 0o755)
        report = self.publish()
        self.assertEqual(report["state"], "published", report)
        self.assertTrue((self.parent / "target/fixture-build-output").is_file())
        self.assertFalse((self.tool / "target").exists())
        self.assertFalse((self.source / "target").exists())
        self.assertEqual(os.environ["RUSTUP_TOOLCHAIN"], "nightly-2025-01-01")
        self.assertEqual(os.environ["RUSTUP_AUTO_INSTALL"], "1")
        self.assertEqual(git(self.tool, "status", "--porcelain"), "")

    def test_explicit_existing_target_reaches_both_scorecard_preparation_commands(self):
        target = self.root / "existing-target"
        target.mkdir()
        cargo = self.root / "bin/cargo"
        cargo.write_text(cargo.read_text().replace(str(self.parent / "target"), str(target)))
        compiler = self.root / "bin/rust-script"
        compiler.write_text(compiler.read_text().replace(
            "import os, sys\n", "import os, sys\n"
            f"assert os.environ['CARGO_TARGET_DIR'] == {str(target)!r}\n"
            f"open({str(target / 'prepared')!r}, 'w').write('existing target only')\n"))
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target)}):
            tool_commit, compiler_name = mirror.prepare_scorecard(
                self.source, self.target, tool_checkout=self.tool,
                state_root=self.parent, net_wrapper="/usr/bin/env")
        self.assertEqual(git(self.tool, "rev-parse", "HEAD"), tool_commit)
        self.assertEqual("nightly-2026-07-12-x86_64-unknown-linux-gnu", compiler_name)
        self.assertEqual("existing target only", (target / "prepared").read_text())
        self.assertFalse((self.parent / "target").exists())
        self.assertFalse((self.tool / "target").exists())

    def test_invalid_explicit_target_refuses_before_compiler_execution(self):
        ordinary_file = self.root / "not-a-target"
        ordinary_file.write_text("preserve")
        for value in ("", "relative-target", str(self.root / "missing-target"), str(ordinary_file)):
            with self.subTest(value=value), mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": value}), \
                    mock.patch.object(mirror, "_network") as compiler, \
                    self.assertRaisesRegex(mirror.MirrorRefused, "explicit CARGO_TARGET_DIR"):
                mirror.prepare_scorecard(self.source, self.target, tool_checkout=self.tool,
                                         state_root=self.parent, net_wrapper="/usr/bin/env")
            compiler.assert_not_called()
        self.assertEqual("preserve", ordinary_file.read_text())
        self.assertFalse((self.root / "missing-target").exists())

    def test_unavailable_tool_compiler_keeps_published_rows_and_failed_derivation(self):
        self.append("compiler-unavailable")
        with mock.patch.object(
            mirror.rust_toolchain, "selected_toolchain",
            side_effect=mirror.rust_toolchain.ToolchainRefused("declared compiler unavailable"),
        ):
            report = self.publish()
        self.assertEqual(report["state"], "derived-failed")
        self.assertIn("declared compiler unavailable", report["scorecard"]["error"])
        self.assertEqual(self.remote_rows(), [row("compiler-unavailable")])
        self.assertEqual(git(self.remote, "show", "main:scorecard/SCORECARD.md"), "legacy scorecard")

    def test_a_published_row_is_found_by_its_id_once_the_local_copy_is_gone(self):
        # append_cell_rows reads only the published rows of the batch's ids.
        # With the retained copy reaped, the published row alone makes an
        # exact replay a no-op and refuses a different body under its id.
        self.append("run-p")
        self.publish(publish_only=True)
        self.publish(publish_only=True)  # reaps the local published/ copy
        spool = series.spool_dir(self.parent)
        self.assertEqual([], list(spool.glob("*.jsonl")) + list((spool / "published").glob("*.jsonl")))
        series.PUBLISHED_ROW_LOOKUPS.clear()
        with mock.patch.object(series, "published_snapshot_at",
                               side_effect=AssertionError("read every published row")):
            self.assertEqual(0, series.append_cell_rows(
                self.parent, spool / "replay.jsonl", [row("run-p")]))
            with self.assertRaisesRegex(series.SeriesError, "already records a different row"):
                series.append_cell_rows(self.parent, spool / "changed.jsonl",
                                        [row("run-p", "failed")])
            self.assertEqual(1, series.append_cell_rows(
                self.parent, spool / "new.jsonl", [row("run-q")]))
        self.assertEqual([1, 1, 0], [lookup["found"] for lookup in series.PUBLISHED_ROW_LOOKUPS])
        self.assertEqual([], list(spool.glob("replay.jsonl")) + list(spool.glob("changed.jsonl")))
        self.assertEqual([row("run-q")], series.read_shard(spool / "new.jsonl"))

    def test_two_concurrent_appenders_and_publishers_preserve_both_events_once(self):
        barrier = threading.Barrier(2)
        def append_and_publish(name):
            barrier.wait()
            self.append(name)
            return self.publish(publish_only=True)
        with ThreadPoolExecutor(max_workers=2) as pool:
            reports = list(pool.map(append_and_publish, ["run-a", "run-b"]))
        self.assertEqual([row("run-a"), row("run-b")], self.remote_rows())
        for report in reports:
            git(self.remote, "merge-base", "--is-ancestor", report["commit"], "main")
        self.assertEqual(self.parent_commit, git(self.parent, "rev-parse", "HEAD"))

    def test_slow_concurrent_compiler_preparation_does_not_hold_publication_lock(self):
        ready = threading.Barrier(3)
        release = threading.Event()
        original = mirror._network

        def slow_prepare(command, **kwargs):
            if command[0] == "rust-script" and command[-1] == "--help":
                self.assertNotIn(mirror.LOCK_FD_ENV, kwargs["environment"])
                self.assertNotIn(series.LEDGER_ROOT_ENV, kwargs["environment"])
                self.assertEqual(kwargs.get("pass_fds", ()), ())
                ready.wait(15)
                self.assertTrue(release.wait(15), "compiler preparation was not released")
            return original(command, **kwargs)

        for name in ("prepared-a", "prepared-b"):
            self.append(name)
        with mock.patch.object(mirror, "_network", side_effect=slow_prepare), \
                ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(self.publish) for _ in range(2)]
            try:
                ready.wait(15)
                # Both genuine fixture child preparations are still blocked.
                # The real writer lock must remain available to another owner.
                with mirror.publication_writer(self.parent, net_wrapper="/usr/bin/env"):
                    self.assertTrue(all(not future.done() for future in futures))
                    self.assertEqual([], self.remote_rows())
            finally:
                release.set()
            reports = [future.result(timeout=30) for future in futures]
        self.assertTrue(all(report["state"] == "published" for report in reports), reports)
        self.assertEqual([row("prepared-a"), row("prepared-b")], self.remote_rows())

    def test_compiler_preparation_failure_preserves_raw_publication_and_retry(self):
        self.append("prepare-refused")
        original = mirror._network

        def refuse_prepare(command, **kwargs):
            if command[-1] == "--help":
                raise mirror.MirrorRefused("actual compiler preparation failed")
            return original(command, **kwargs)

        with mock.patch.object(mirror, "_network", side_effect=refuse_prepare):
            report = self.publish()
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("compiler preparation failed", report["scorecard"]["error"])
        self.assertEqual([row("prepare-refused")], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        self.assertEqual("published", self.publish()["state"])
        self.assertEqual([row("prepare-refused")], self.remote_rows())

    def test_checker_preparation_failure_keeps_spool_without_taking_writer_lock(self):
        spool = self.append("checker-prepare-refused")
        original = spool.read_bytes()
        with mock.patch.object(series, "_run_shared_checker", return_value=
                               subprocess.CompletedProcess(["checker-fixture"], 2, "", "compile failed")), \
                mock.patch.object(mirror, "publication_writer",
                                  side_effect=AssertionError("failed preparation took writer lock")):
            with self.assertRaisesRegex(series.SeriesError, "cannot prepare.*compile failed"):
                self.publish()
        self.assertEqual(original, spool.read_bytes())
        self.assertEqual([], self.remote_rows())
        self.assertEqual("published", self.publish()["state"])
        self.assertEqual([row("checker-prepare-refused")], self.remote_rows())

    def test_legacy_export_does_not_relabel_or_rewrite_the_existing_scorecard(self):
        pending = self.append("unrelated-pending-result")
        pending_bytes = pending.read_bytes()
        seed = self.root / "seed"
        old_metadata = '{"scorecard_hermit_commit":"preserve the original source"}\n'
        (seed / "latest.json").write_text(old_metadata)
        git(seed, "add", "latest.json")
        git(seed, "commit", "-qm", "existing scorecard identity")
        git(seed, "push", "-q", "origin", "main")
        cells = self.source / "ci/compat-envelope/cells.json"
        cells.parent.mkdir(parents=True)
        cells.write_text('{"exact":"legacy source bytes"}\n')
        git(self.source, "add", "ci")
        git(self.source, "commit", "-qm", "legacy source document")
        self.target = git(self.source, "rev-parse", "HEAD")
        blob = git(self.source, "rev-parse", "HEAD:ci/compat-envelope/cells.json")
        tool = self.tool / "ci/compat-envelope/scorecard.rs"
        tool.write_text("#!/usr/bin/python3\nimport os, pathlib, sys\n"
                        + HELP_FIXTURE +
                        "assert sys.argv[1:] == ['export-legacy']\n"
                        "os.fstat(int(os.environ['DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD']))\n"
                        "p=pathlib.Path(os.environ['DEV_HERMIT_TEST_LEDGER_ROOT'])/'scorecard/legacy'\n"
                        "p.mkdir()\n"
                        f"(p/{(blob+'.json')!r}).write_bytes(pathlib.Path('ci/compat-envelope/cells.json').read_bytes())\n"
                        f"(p/{(blob+'.identity.json')!r}).write_text('{{}}\\n')\n")
        git(self.tool, "add", "ci")
        git(self.tool, "commit", "-qm", "legacy export transport fixture")
        # A scorecard command publishes only a commit a validation measured.
        writer = self.parent / mirror.WRITER_RELATIVE
        writer_head = git(writer, "rev-parse", "HEAD") if writer.exists() else None
        with self.assertRaisesRegex(mirror.MirrorRefused, "which no validation measured"):
            self.publish(scorecard_command="export-legacy")
        self.assertEqual(writer_head, git(writer, "rev-parse", "HEAD") if writer.exists() else None)
        self.assertEqual(pending_bytes, pending.read_bytes())
        runs = self.parent / "ignored" / "validate" / "runs"
        runs.mkdir(parents=True, exist_ok=True)
        (runs / "validate-legacy-export.json").write_text(json.dumps({"target": self.target}))
        with mock.patch.object(mirror.retention_render, "prepare", side_effect=AssertionError(
                "a legacy-only export must never require or relabel a parity summary")):
            report = self.publish(scorecard_command="export-legacy")
        self.assertEqual("published", report["legacy_export"]["state"], report)
        self.assertNotIn("scorecard", report)
        self.assertNotIn("rows", report)
        self.assertEqual(pending_bytes, pending.read_bytes())
        self.assertEqual([], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        self.assertEqual('{"legacy":"preserve"}', git(self.remote, "show", "main:scorecard/cells.json"))
        self.assertEqual(f"scorecard/legacy/{blob}.identity.json\nscorecard/legacy/{blob}.json",
                         git(self.remote, "diff-tree", "--no-commit-id", "--name-only", "-r", "main"))
        self.assertEqual(old_metadata.strip(), git(self.remote, "show", "main:latest.json"))

    def test_rejected_push_retains_spool_and_named_writer_commit(self):
        batch = self.append("reject")
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\necho deliberate-push-refusal >&2\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaisesRegex(mirror.MirrorRefused, "spool retained"):
            self.publish(publish_only=True, attempts=1)
        self.assertEqual([row("reject")], [json.loads(line) for line in batch.read_text().splitlines()])
        self.assertEqual([], self.remote_rows())
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertNotEqual(git(self.remote, "rev-parse", "main"), git(writer, "rev-parse", "HEAD"))
        refused_commit = git(writer, "rev-parse", "HEAD")
        hook.unlink()
        recovered = self.publish(publish_only=True)
        self.assertEqual([row("reject")], self.remote_rows())
        self.assertEqual(refused_commit, git(writer, "rev-parse", recovered["raw_publication"]["recovered_ref"]))
        self.assertFalse(batch.exists())

    def test_rejected_push_that_starts_a_new_part_is_recovered(self):
        # A month's next part (https://github.com/rrnewton/dev-hermit/issues/453)
        # is series work the publisher wrote, so recovery must accept it.
        self.append("run-one")
        self.publish(publish_only=True)
        part_zero = git(self.remote, "show", "main:series/hermit/fixture/2026-09.jsonl") + "\n"
        width = len((json.dumps(row("reject"), sort_keys=True) + "\n").encode())
        self.enterContext(mock.patch.object(
            mirror.publisher, "SERIES_SHARD_CEILING_BYTES", len(part_zero.encode()) + width - 1,
        ))
        batch = self.append("reject")
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaisesRegex(mirror.MirrorRefused, "spool retained"):
            self.publish(publish_only=True, attempts=1)
        writer = self.parent / mirror.WRITER_RELATIVE
        refused_commit = git(writer, "rev-parse", "HEAD")
        self.assertEqual(
            "series/hermit/fixture/2026-09.part-0001.jsonl",
            git(writer, "diff-tree", "--no-commit-id", "--name-only", "-r", refused_commit),
        )
        hook.unlink()
        recovered = self.publish(publish_only=True)
        self.assertEqual([row("run-one")], self.remote_rows())
        self.assertEqual(
            [row("reject")],
            [json.loads(line) for line in git(
                self.remote, "show", "main:series/hermit/fixture/2026-09.part-0001.jsonl",
            ).splitlines()],
        )
        self.assertEqual(refused_commit, git(writer, "rev-parse", recovered["raw_publication"]["recovered_ref"]))
        self.assertFalse(batch.exists())

    def squash(self, *, tree: str | None = None) -> tuple[str, str]:
        """Replace remote main with a ledger snapshot, as the weekly squash does."""
        old = git(self.remote, "rev-parse", "main")
        snapshot = git(self.remote, "commit-tree", tree or git(self.remote, "rev-parse", "main^{tree}"),
                       "-m", f"ledger snapshot of {old}", "-m", f"{mirror.SNAPSHOT_TRAILER}: {old}")
        git(self.remote, "update-ref", "refs/heads/main", snapshot, old)
        return old, snapshot

    def test_a_writer_publishes_after_main_is_squashed(self):
        # https://github.com/rrnewton/dev-hermit/issues/562: after a squash the
        # writer's HEAD is the old tip, which is no ancestor of main.
        self.append("before")
        self.publish(publish_only=True)
        old, snapshot = self.squash()
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual(old, git(writer, "rev-parse", "HEAD"))
        self.append("after")
        self.publish(publish_only=True)
        self.assertEqual([row("before"), row("after")], self.remote_rows())
        self.assertEqual(snapshot, git(self.remote, "rev-list", "--max-parents=0", "main"))
        git(self.remote, "merge-base", "--is-ancestor", snapshot, "main")

    def test_a_rejected_push_is_recovered_after_main_is_squashed(self):
        # The refused commit's parent is the old tip, so it shares no history
        # with the snapshot; recovery used to raise on the missing merge base.
        self.append("before")
        self.publish(publish_only=True)
        batch = self.append("reject")
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaisesRegex(mirror.MirrorRefused, "spool retained"):
            self.publish(publish_only=True, attempts=1)
        writer = self.parent / mirror.WRITER_RELATIVE
        refused = git(writer, "rev-parse", "HEAD")
        hook.unlink()
        _old, snapshot = self.squash()
        recovered = self.publish(publish_only=True)
        self.assertEqual([row("before"), row("reject")], self.remote_rows())
        self.assertEqual(refused, git(writer, "rev-parse", recovered["raw_publication"]["recovered_ref"]))
        self.assertEqual(snapshot, git(self.remote, "rev-list", "--max-parents=0", "main"))
        self.assertFalse(batch.exists())

    def test_a_snapshot_whose_tree_differs_from_its_old_tip_folds_nothing_in(self):
        self.append("before")
        self.publish(publish_only=True)
        self.append("reject")
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaisesRegex(mirror.MirrorRefused, "spool retained"):
            self.publish(publish_only=True, attempts=1)
        writer = self.parent / mirror.WRITER_RELATIVE
        refused = git(writer, "rev-parse", "HEAD")
        hook.unlink()
        # A snapshot naming the old tip but holding another tree (here main's
        # before the old tip) is not a snapshot.
        old, _snapshot = self.squash(tree=git(self.remote, "rev-parse", "main~1^{tree}"))
        git(writer, "fetch", "-q", "origin", "main")
        self.assertEqual([], mirror.snapshot_tips(writer))
        self.assertFalse(mirror._published(writer, old))
        with self.assertRaisesRegex(mirror.MirrorRefused, "shares no history"):
            mirror.recover_spooled_commit(writer, self.parent)
        self.assertEqual(refused, git(writer, "rev-parse", "HEAD"))

    def test_snapshot_tips_follow_a_chain_of_squashes(self):
        self.append("one")
        self.publish(publish_only=True)
        first_tip, _ = self.squash()
        self.append("two")
        self.publish(publish_only=True)
        second_tip, _ = self.squash()
        writer = self.parent / mirror.WRITER_RELATIVE
        git(writer, "fetch", "-q", "origin", "main")
        self.assertEqual([second_tip, first_tip], mirror.snapshot_tips(writer))
        self.assertTrue(mirror._published(writer, first_tip))
        self.assertEqual([row("one"), row("two")], self.remote_rows())

    def test_one_rejected_push_retries_append_without_duplicate(self):
        self.append("retry")
        marker = self.remote / "rejected-once"
        hook = self.remote / "hooks/pre-receive"
        hook.write_text(f"#!/bin/sh\nif [ ! -e '{marker}' ]; then touch '{marker}'; exit 1; fi\n")
        hook.chmod(0o755)
        report = self.publish(publish_only=True)
        self.assertEqual(2, report["raw_publication"]["attempts"])
        self.assertEqual([row("retry")], self.remote_rows())

    def test_recovery_refuses_a_changed_spooled_body_without_reset(self):
        batch = self.append("pending")
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaises(mirror.MirrorRefused):
            self.publish(publish_only=True, attempts=1)
        writer = self.parent / mirror.WRITER_RELATIVE
        refused = git(writer, "rev-parse", "HEAD")
        batch.write_text(json.dumps(row("pending", "diverged")) + "\n")
        hook.unlink()
        with self.assertRaisesRegex(mirror.MirrorRefused, "unchanged spooled append"):
            self.publish(publish_only=True)
        self.assertEqual(refused, git(writer, "rev-parse", "HEAD"))
        self.assertEqual([], self.remote_rows())
        self.assertTrue(batch.exists())

    def test_recovery_refuses_rewriting_a_prior_published_event(self):
        self.append("existing")
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        shard = writer / "series/hermit/fixture/2026-09.jsonl"
        shard.write_text(json.dumps(row("existing", "diverged")) + "\n")
        git(writer, "add", "series")
        git(writer, "commit", "-qm", "unpublished prior-event rewrite opponent")
        refused = git(writer, "rev-parse", "HEAD")
        with self.assertRaisesRegex(mirror.MirrorRefused, "unchanged spooled append"):
            self.publish(publish_only=True)
        self.assertEqual(refused, git(writer, "rev-parse", "HEAD"))
        self.assertEqual([row("existing")], self.remote_rows())

    def test_same_id_different_body_is_refused_and_original_survives(self):
        self.append("identity")
        self.publish(publish_only=True)
        with self.assertRaisesRegex(series.SeriesError, "different row"):
            self.append("identity", "diverged")
        self.assertEqual([row("identity")], self.remote_rows())

    def test_derived_failure_keeps_raw_publication_and_legacy_bytes(self):
        self.append("raw-survives")
        with mock.patch.object(mirror, "build_scorecard", side_effect=mirror.MirrorRefused("derived refusal")):
            report = self.publish()
        self.assertEqual("derived-failed", report["state"])
        self.assertEqual([row("raw-survives")], self.remote_rows())
        git(self.remote, "merge-base", "--is-ancestor", report["commit"], "main")
        self.assertEqual('{"legacy":"preserve"}', git(self.remote, "show", "main:scorecard/cells.json"))

    def test_repository_identity_disambiguates_old_parent_commits(self):
        old = series.published_snapshot_at(self.parent, self.parent_commit, repository=series.PARENT_REPOSITORY)
        self.assertEqual([row("historical-parent")], old.rows)
        current = series.published_snapshot_at(self.parent, "origin/main", repository=mirror.REMOTE_URL)
        self.assertEqual([], current.rows)
        self.assertEqual(mirror.REMOTE_URL, current.repository)
        with self.assertRaises(series.SeriesError):
            series.published_snapshot_at(self.parent, self.parent_commit, repository=mirror.REMOTE_URL)
        with self.assertRaises(series.SeriesError):
            series.published_repository(self.parent, "https://example.invalid/other.git")

    def test_remote_identity_mismatch_refuses_before_publication(self):
        self.append("keep")
        git(self.ledger, "config", "remote.origin.url", "https://example.invalid/other.git")
        with self.assertRaisesRegex(series.SeriesError, "unexpected origin"):
            self.publish(publish_only=True)
        self.assertEqual([], self.remote_rows())
        self.assertTrue(list(series.spool_dir(self.parent).glob("*.jsonl")))

    def test_measured_head_move_refuses_derived_delivery_after_raw_publication(self):
        self.append("head-move")
        run = mirror._run
        renders = []

        def move_after_scorecard(command, **kwargs):
            result = run(command, **kwargs)
            # The renderer is run directly, never through the network wrapper.
            if (command[0] == "rust-script" and str(self.tool / "ci/compat-envelope/scorecard.rs") in command
                    and command[-1] != "--help"):
                renders.append(command)
                git(self.source, "commit", "--allow-empty", "-qm", "concurrent source change")
            return result

        with mock.patch.object(mirror, "_run", side_effect=move_after_scorecard):
            report = self.publish()
        self.assertEqual(1, len(renders))
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("source HEAD changed", report["scorecard"]["error"])
        self.assertEqual([row("head-move")], self.remote_rows())
        # The refused render stayed in its discarded copy.
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("legacy scorecard\n", (writer / "scorecard/SCORECARD.md").read_text())
        self.assertEqual("", git(writer, "status", "--porcelain"))

    def _record_run_timeouts(self, **publish_kwargs):
        """Publish once, returning the timeout each render and every other child received."""
        run = mirror._run
        renders, others = [], []

        def record(command, **kwargs):
            if (command[0] == "rust-script" and str(self.tool / "ci/compat-envelope/scorecard.rs") in command
                    and command[-1] != "--help"):
                renders.append(kwargs.get("timeout"))
            else:
                others.append(kwargs.get("timeout", mirror.DEFAULT_TIMEOUT_SECONDS))
            return run(command, **kwargs)

        with mock.patch.object(mirror, "_run", side_effect=record):
            report = self.publish(**publish_kwargs)
        return report, renders, others

    def test_scorecard_render_gets_the_lifecycle_share_not_the_per_command_default(self):
        # A 216,929-row ledger rendered in 1,066 s on 2026-10-03; the old
        # 900 s per-command default could never let that render finish.
        self.append("render-budget")
        report, renders, others = self._record_run_timeouts()
        self.assertEqual("published", report["state"], report)
        self.assertEqual([float(mirror.LIFECYCLE_TIMEOUT_SECONDS - mirror.SCORECARD_RESERVE_SECONDS)], renders)
        self.assertGreater(renders[0], 1066 * 1.5)
        self.assertTrue(others)
        self.assertEqual({mirror.DEFAULT_TIMEOUT_SECONDS}, set(others))

    def test_scorecard_render_is_bounded_by_the_remaining_lifecycle_deadline(self):
        self.append("render-deadline")
        lifetime = 3_600
        before = time.monotonic_ns()
        with mock.patch.object(mirror, "_verify_lifecycle_worker") as verify:
            report, renders, others = self._record_run_timeouts(
                lifecycle_deadline_ns=before + lifetime * 1_000_000_000)
        verify.assert_called()
        self.assertEqual("published", report["state"], report)
        self.assertEqual(1, len(renders))
        elapsed = (time.monotonic_ns() - before) / 1e9
        # The remainder at render time, less the reserve: never more than what
        # was left, and nowhere near the 900 s default or the fixed share.
        self.assertLessEqual(renders[0], lifetime - mirror.SCORECARD_RESERVE_SECONDS)
        self.assertGreater(renders[0], lifetime - mirror.SCORECARD_RESERVE_SECONDS - elapsed - 1)
        self.assertEqual({mirror.DEFAULT_TIMEOUT_SECONDS}, set(others))

    def test_a_short_deadline_halves_the_reserve_and_still_renders(self):
        # A caller deadline under twice the reserve (the 600 s retirement path,
        # the 30 s bounded fixtures) splits what is left; it does not refuse.
        self.append("render-short")
        lifetime = 120
        before = time.monotonic_ns()
        with mock.patch.object(mirror, "_verify_lifecycle_worker"):
            report, renders, _others = self._record_run_timeouts(
                lifecycle_deadline_ns=before + lifetime * 1_000_000_000)
        elapsed = (time.monotonic_ns() - before) / 1e9
        self.assertEqual("published", report["state"], report)
        self.assertEqual(1, len(renders))
        self.assertLessEqual(renders[0], lifetime / 2)
        self.assertGreater(renders[0], (lifetime - elapsed) / 2 - 1)

    def test_a_passed_deadline_refuses_the_render_and_reports_it(self):
        self.append("render-exhausted")
        with mock.patch.object(mirror, "_verify_lifecycle_worker"):
            report, renders, _others = self._record_run_timeouts(
                lifecycle_deadline_ns=time.monotonic_ns() - 1_000_000_000)
        # The render never starts; the raw rows still publish and the refusal
        # is reported rather than the worker being killed at the deadline.
        self.assertEqual([], renders)
        self.assertEqual("derived-failed", report["state"], report)
        self.assertIn("renderer was not started", report["scorecard"]["error"])
        self.assertEqual([row("render-exhausted")], self.remote_rows())

    def test_unknown_dirty_writer_is_retained_without_reset(self):
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        unknown = writer / "unpublished-work"
        unknown.write_bytes(b"preserve this work\n")
        batch = self.append("pending")
        with self.assertRaisesRegex(mirror.MirrorRefused, "unpublished work"):
            self.publish(publish_only=True)
        self.assertEqual(b"preserve this work\n", unknown.read_bytes())
        self.assertTrue(batch.exists())
        self.assertEqual([], self.remote_rows())

    def test_dirty_tool_inputs_refuse_without_mislabeling_the_raw_publication(self):
        helper = self.tool / "ci/manifest-plan/src/lib.rs"
        helper.parent.mkdir(parents=True)
        helper.write_text("// committed helper\n")
        verdict = self.tool / "hermit-cli/src/canonical_verdict.rs"
        verdict.parent.mkdir(parents=True)
        verdict.write_text("// committed verdict semantics\n")
        git(self.tool, "add", "ci/manifest-plan", "hermit-cli")
        git(self.tool, "commit", "-qm", "fixture helper input")
        for changed, staged in ((helper, False), (helper, True), (verdict, False)):
            identifier = f"dirty-tool-{changed.stem}-{staged}"
            with self.subTest(path=changed, staged=staged):
                changed.write_text("// changed compiler input\n")
                if staged:
                    git(self.tool, "add", str(changed.relative_to(self.tool)))
                self.append(identifier)
                report = self.publish()
                self.assertEqual("derived-failed", report["state"])
                self.assertIn("tool inputs have uncommitted changes", report["scorecard"]["error"])
                self.assertNotIn("tool_hermit_commit", report)
                self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
                git(self.tool, "restore", "--staged", "--worktree", "--", "ci/manifest-plan", "hermit-cli")
        self.assertCountEqual([row("dirty-tool-lib-False"), row("dirty-tool-lib-True"),
                              row("dirty-tool-canonical_verdict-False")], self.remote_rows())
        # Source catalogue/history belongs to the measured checkout, not the
        # compiler input closure: unrelated data dirt must not block delivery.
        (self.tool / "ci/compat-envelope/cells.json").write_text("unrelated data\n")
        self.assertEqual("published", self.publish()["state"])

    def test_normal_completion_records_real_ledger_commit_without_changing_verdict(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub"))
        import start_unit
        import run_registry

        # This launches the production finalizer/CLI and an executable Python
        # scorecard fixture. The row checker and canonical producer query are
        # doubles; this verifies Git delivery, not finalized proof validity.
        # The schema-10 row reaches portable proof preparation, whose real
        # publisher path the portable_fixture tests exercise; here it is a
        # double that records the exact row it received and prepares nothing.
        portable_calls = self.root / "portable-calls.jsonl"
        bootstrap = (
            "import runpy,sys,subprocess;"
            "expected_run_id=sys.argv.pop(1);"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher,validate_rows;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            "r={'repo':'hermit','run_id':expected_run_id,"
            "'record_id':'fixture-final','commit':sys.argv[sys.argv.index('--target')+1],"
            "'cwd':sys.argv[sys.argv.index('--source-checkout')+1],'schema_version':10,'started_at':'2026-09-21T12:00:00Z','finished_at':'2026-09-21T12:10:00Z'};"
            "validate_rows.rows=lambda **k:[r];"
            # Decline the cached and the streamed read, so the producer
            # lookup reads rows().
            "import row_replay_cache;row_replay_cache.run_row_lines=lambda *a,**k:None;"
            "validate_rows._stream_preserved_rows=lambda *a,**k:False;"
            "validate_rows.canonical_finalized_row=lambda **k:r;"
            "import json,mirror;"
            f"calls={str(portable_calls)!r};"
            "mirror.prepare_portable_proof=lambda parent,row,**k:"
            "open(calls,'a').write(json.dumps(json.loads(row),sort_keys=True)+'\\n') and None;"
            "sys.argv=sys.argv[1:];assert sys.argv[0]==mirror.__file__,sys.argv[0];"
            "raise SystemExit(mirror.main())"
        )
        calls = []

        def native_run(command, **kwargs):
            calls.append(command)
            return subprocess.run(
                bounded_fixture_command([sys.executable, "-B", "-c", bootstrap,
                    expected_run_id, *command[1:], "--tool-checkout", str(self.tool)]),
                cwd=kwargs.get("cwd"), capture_output=True, text=True, timeout=30,
            )

        # Since 68e56d4c002af5dbe9ed055caa186cb89a8acfd5 ("Require explicit bot
        # GitHub routing") the finalizer restores the launch's GitHub executable
        # from the run record and refuses without one. Publication never needs
        # GitHub, so the record names a stub that fails loudly if called, and the
        # caller's own FLEET_REAL_GH is removed so it cannot supply the route.
        no_gh = self.root / "no-gh"
        no_gh.write_text("#!/bin/sh\necho 'GitHub must not be called' >&2\nexit 97\n")
        no_gh.chmod(0o755)
        for code, verdict in [(0, "VALIDATED"), (start_unit.EXIT_FAILED, "FAILED")]:
            with self.subTest(verdict=verdict):
                expected_run_id = "validate-" + verdict.lower()
                self.append(verdict)
                record_path = self.parent / f"{verdict}.json"
                record = {
                    "schema_version": run_registry.SCHEMA_VERSION,
                    "producer": "normal-completion-publication-fixture",
                    "repo": "rrnewton/hermit", "checkout": str(self.source),
                    "target": self.target, "canonical_verdict": verdict,
                    "unit": expected_run_id + ".service",
                    "wrapper_exit_code": code, "e2e_result_root": str(self.root / "results"),
                    "github_executable": str(no_gh),
                }
                run_registry.write_record(record_path, record)
                with mock.patch.dict(os.environ):
                    os.environ.pop("FLEET_REAL_GH", None)
                    result = start_unit.finalize_ledger_mirror(
                        self.parent, mirror.TOOL_ROOT, record_path, record=record,
                        run=native_run,
                    )
                self.assertTrue(start_unit.cell_ledger_published(result), result)
                self.assertEqual(verdict, result["canonical_verdict"])
                self.assertEqual(code, result["wrapper_exit_code"])
                commit = result["ledger_mirror"]["commit"]
                git(self.remote, "merge-base", "--is-ancestor", commit, "main")
                self.assertEqual(result, run_registry.read_record(record_path))
                self.assertIn("--results", calls[-1])
                self.assertEqual(expected_run_id,
                                 calls[-1][calls[-1].index("--finalized-run-id") + 1])
                prepared = [json.loads(line) for line in portable_calls.read_text().splitlines()]
                self.assertEqual((10, expected_run_id), (prepared[-1]["schema_version"], prepared[-1]["run_id"]))
        self.assertEqual(2, len(portable_calls.read_text().splitlines()))
        self.assertEqual({"VALIDATED", "FAILED"}, {row["event_id"] for row in self.remote_rows()})
        self.assertEqual("", git(self.source, "status", "--porcelain"))

    def test_finalized_run_record_counts_only_that_runs_quarantined_inputs(self):
        # The run record is where a validate's refused inputs are counted.
        # start_unit copies the mirror's raw_publication into ledger_mirror and
        # names the run by the same suffix-free unit it exported as E2E_RUN_ID,
        # which is the --run-id the validate gave append-cells.
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub"))
        import start_unit
        import run_registry

        expected_run_id = "validate-quarantine"
        # As in the normal-completion test above, the schema-10 row reaches
        # portable proof preparation; here it is a double that records the
        # exact row it received and prepares nothing.
        portable_calls = self.root / "portable-calls.jsonl"
        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher,validate_rows;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            f"r={{'repo':'hermit','run_id':{expected_run_id!r},"
            "'record_id':'fixture-final','commit':sys.argv[sys.argv.index('--target')+1],"
            "'cwd':sys.argv[sys.argv.index('--source-checkout')+1],'schema_version':10,'started_at':'2026-09-21T12:00:00Z','finished_at':'2026-09-21T12:10:00Z'};"
            "validate_rows.rows=lambda **k:[r];"
            # Decline the cached and the streamed read, so the producer
            # lookup reads rows().
            "import row_replay_cache;row_replay_cache.run_row_lines=lambda *a,**k:None;"
            "validate_rows._stream_preserved_rows=lambda *a,**k:False;"
            "validate_rows.canonical_finalized_row=lambda **k:r;"
            "import json,mirror;"
            f"calls={str(portable_calls)!r};"
            "mirror.prepare_portable_proof=lambda parent,row,**k:"
            "open(calls,'a').write(json.dumps(json.loads(row),sort_keys=True)+'\\n') and None;"
            "sys.argv=sys.argv[1:];assert sys.argv[0]==mirror.__file__,sys.argv[0];"
            "raise SystemExit(mirror.main())"
        )

        def native_run(command, **kwargs):
            return subprocess.run(
                bounded_fixture_command([sys.executable, "-B", "-c", bootstrap,
                                         *command[1:], "--tool-checkout", str(self.tool)]),
                cwd=kwargs.get("cwd"), env=kwargs.get("env"), capture_output=True, text=True,
                timeout=30,
            )

        # Publication never needs GitHub; a call would fail this test loudly.
        no_gh = self.root / "no-gh"
        no_gh.write_text("#!/bin/sh\necho 'GitHub must not be called' >&2\nexit 97\n")
        no_gh.chmod(0o755)
        self.append("QUARANTINED")
        mine = self.quarantine(expected_run_id, b'{"schema": 4, "truncated')
        other = self.quarantine("validate-other", b"another run's refusal")
        series.append_quarantine(self.parent, expected_run_id, [mine])
        series.append_quarantine(self.parent, "validate-other", [other])
        record_path = self.parent / "quarantine-run.json"
        record = {
            "schema_version": run_registry.SCHEMA_VERSION,
            "producer": "normal-completion-publication-fixture",
            "repo": "rrnewton/hermit", "checkout": str(self.source),
            "target": self.target, "canonical_verdict": "VALIDATED",
            "unit": expected_run_id + ".service", "wrapper_exit_code": 0,
            "e2e_result_root": str(self.root / "results"),
            "github_executable": str(no_gh),
        }
        run_registry.write_record(record_path, record)
        with mock.patch.dict(os.environ):
            os.environ.pop("FLEET_REAL_GH", None)
            result = start_unit.finalize_ledger_mirror(
                self.parent, mirror.TOOL_ROOT, record_path, record=record, run=native_run,
            )
        self.assertTrue(start_unit.cell_ledger_published(result), result)
        self.assertEqual(result, run_registry.read_record(record_path))
        prepared = [json.loads(line) for line in portable_calls.read_text().splitlines()]
        self.assertEqual([(10, expected_run_id)],
                         [(item["schema_version"], item["run_id"]) for item in prepared])
        counted = result["ledger_mirror"]["raw_publication"]["quarantine"]
        self.assertIsNone(counted["error"], counted)
        self.assertEqual(2, counted["published"])
        self.assertEqual(0, counted["pending_records"])
        # Violating: the other run's refusal is published too but is not this
        # run's, so it must not be counted against it.
        self.assertEqual({"run_id": expected_run_id, "published": 1, "withheld": 0,
                          "unpublished": 0, "records": 1}, counted["run"])
        self.assertEqual({mine["event_id"], other["event_id"]},
                         {item["event_id"] for item in self.remote_quarantine()})

    def spool_refused_series_batch(self) -> Path:
        """A series batch the publisher refuses before touching Git."""
        bogus = dict(row("zz-bogus"), emitted_at="bogus")
        path = series.spool_dir(self.parent) / "zz-bogus.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(bogus, sort_keys=True) + "\n")
        return path

    def test_refused_series_batch_still_publishes_the_quarantine_and_the_record_counts_it(self):
        # A refused series batch used to raise before the quarantine publish
        # ran, stranding every refused input on this host and leaving the run
        # record with state=failed and no count.
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub"))
        import start_unit
        import run_registry

        expected_run_id = "validate-refused-series"
        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher,validate_rows;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            f"r={{'repo':'hermit','run_id':{expected_run_id!r},"
            "'record_id':'fixture-final','commit':sys.argv[sys.argv.index('--target')+1],"
            "'cwd':sys.argv[sys.argv.index('--source-checkout')+1],'started_at':'2026-09-21T12:00:00Z'};"
            "validate_rows.rows=lambda **k:[r];"
            # Decline the cached and the streamed read, so the producer
            # lookup reads rows().
            "import row_replay_cache;row_replay_cache.run_row_lines=lambda *a,**k:None;"
            "validate_rows._stream_preserved_rows=lambda *a,**k:False;"
            "validate_rows.canonical_finalized_row=lambda **k:r;"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )

        def native_run(command, **kwargs):
            return subprocess.run(
                bounded_fixture_command([sys.executable, "-B", "-c", bootstrap,
                                         *command[1:], "--tool-checkout", str(self.tool)]),
                cwd=kwargs.get("cwd"), env=kwargs.get("env"), capture_output=True, text=True,
                timeout=30,
            )

        no_gh = self.root / "no-gh"
        no_gh.write_text("#!/bin/sh\necho 'GitHub must not be called' >&2\nexit 97\n")
        no_gh.chmod(0o755)
        bogus = self.spool_refused_series_batch()
        refused_batch = bogus.read_bytes()
        mine = self.quarantine(expected_run_id, b'{"schema": 4, "truncated')
        series.append_quarantine(self.parent, expected_run_id, [mine])
        record_path = self.parent / "refused-series-run.json"
        record = {
            "schema_version": run_registry.SCHEMA_VERSION,
            "producer": "normal-completion-publication-fixture",
            "repo": "rrnewton/hermit", "checkout": str(self.source),
            "target": self.target, "canonical_verdict": "VALIDATED",
            "unit": expected_run_id + ".service", "wrapper_exit_code": 0,
            "e2e_result_root": str(self.root / "results"),
            "github_executable": str(no_gh),
        }
        run_registry.write_record(record_path, record)
        with mock.patch.dict(os.environ):
            os.environ.pop("FLEET_REAL_GH", None)
            result = start_unit.finalize_ledger_mirror(
                self.parent, mirror.TOOL_ROOT, record_path, record=record, run=native_run,
            )
        mirrored = result["ledger_mirror"]
        # The series failure is still a failure, and the verdict is unchanged.
        self.assertEqual("failed", mirrored["state"], mirrored)
        self.assertIn("month must be YYYY-MM", mirrored["detail"])
        self.assertEqual("VALIDATED", result["canonical_verdict"])
        self.assertEqual(refused_batch, bogus.read_bytes(), "the refused batch stays spooled")
        self.assertEqual([], self.remote_rows())
        # The refused input landed in the ledger anyway ...
        self.assertEqual([series.portable_quarantine_record(mine)], self.remote_quarantine())
        # ... and the durable run record counts it.
        counted = mirrored["raw_publication"]["quarantine"]
        self.assertIsNone(counted["error"], counted)
        self.assertEqual((1, 0), (counted["published"], counted["pending_records"]))
        self.assertEqual({"run_id": expected_run_id, "published": 1, "withheld": 0,
                          "unpublished": 0, "records": 1}, counted["run"])
        self.assertEqual(result, run_registry.read_record(record_path))

    def test_a_failure_reports_quarantine_count_the_record_refuses_keeps_the_failure(self):
        # The run record's schema types a failed mirror's quarantine count
        # strictly, but only its readers check it: the writer stores whatever
        # it is given. An unfit count copied in as-is would make every strict
        # reader refuse the whole record. So it is checked before the copy; the
        # failure is still recorded, and the refusal is named in its detail.
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub"))
        import start_unit
        import run_registry

        run_id = "validate-unfit-count"
        series.append_quarantine(self.parent, run_id, [self.quarantine(run_id, b"not json")])
        fits = mirror.publish_quarantine(self.parent, self.parent, None, net_wrapper="",
                                         attempts=1, run_id=run_id, blocked="fixture")
        self.assertEqual(1, fits["run"]["unpublished"], fits)
        no_gh = self.root / "no-gh"
        no_gh.write_text("#!/bin/sh\necho 'GitHub must not be called' >&2\nexit 97\n")
        no_gh.chmod(0o755)
        cases = [
            ("fits", fits),
            ("negative count", {**fits, "published": -1}),
            ("missing corrupt_files", {k: v for k, v in fits.items() if k != "corrupt_files"}),
            ("run with a boolean count", {**fits, "run": {**fits["run"], "unpublished": True}}),
        ]
        for index, (name, quarantine) in enumerate(cases):
            with self.subTest(name):
                failure = {"state": "failed", "repository": mirror.REMOTE_URL,
                           "error": "MirrorRefused: fixture series failure",
                           "raw_publication": {"quarantine": quarantine}}

                def failed_mirror(command, **kwargs):
                    return subprocess.CompletedProcess(
                        command, 1, json.dumps(failure, sort_keys=True) + "\n",
                        "ledger publication: FAILED: fixture series failure\n")

                record_path = self.parent / f"unfit-count-{index}.json"
                record = {
                    "schema_version": run_registry.SCHEMA_VERSION,
                    "producer": "normal-completion-publication-fixture",
                    "repo": "rrnewton/hermit", "checkout": str(self.source),
                    "target": self.target, "canonical_verdict": "VALIDATED",
                    "unit": run_id + ".service", "wrapper_exit_code": 0,
                    "e2e_result_root": str(self.root / "results"),
                    "github_executable": str(no_gh),
                }
                run_registry.write_record(record_path, record)
                with mock.patch.dict(os.environ):
                    os.environ.pop("FLEET_REAL_GH", None)
                    result = start_unit.finalize_ledger_mirror(
                        self.parent, mirror.TOOL_ROOT, record_path, record=record,
                        run=failed_mirror,
                    )
                self.assertEqual(result, run_registry.read_record(record_path))
                mirrored = result["ledger_mirror"]
                self.assertEqual("failed", mirrored["state"], mirrored)
                self.assertIn("fixture series failure", mirrored["detail"])
                self.assertEqual("VALIDATED", result["canonical_verdict"])
                # What was written is what the record's own schema accepts.
                run_registry._ledger_mirror(mirrored)
                if name == "fits":
                    self.assertEqual({"quarantine": fits}, mirrored["raw_publication"])
                    self.assertNotIn("quarantine outcome not recorded", mirrored["detail"])
                else:
                    self.assertNotIn("raw_publication", mirrored)
                    self.assertIn("quarantine outcome not recorded: "
                                  "validation-run-handle-ledger_mirror.raw_publication",
                                  mirrored["detail"])

    def test_failed_series_push_keeps_its_commit_and_still_publishes_the_quarantine(self):
        hook = self.remote / "hooks/pre-receive"
        hook.write_text(
            "#!/bin/sh\nwhile read old new ref; do\n"
            " if git diff --name-only \"$old\" \"$new\" | grep -q '^series/'; then\n"
            "  echo deliberate-series-refusal >&2; exit 1\n fi\ndone\nexit 0\n"
        )
        hook.chmod(0o755)
        batch = self.append("run-push-refused")
        record = self.quarantine("run-push-refused", b"not json")
        series.append_quarantine(self.parent, "run-push-refused", [record])
        writer = self.parent / mirror.WRITER_RELATIVE
        with self.assertRaises(mirror.SeriesPublicationFailed) as caught:
            self.publish(publish_only=True, attempts=1)
        self.assertIn("deliberate-series-refusal", str(caught.exception))
        quarantine = caught.exception.quarantine
        self.assertEqual((1, None, 0), (quarantine["published"], quarantine["error"],
                                        quarantine["pending_records"]))
        self.assertEqual([series.portable_quarantine_record(record)], self.remote_quarantine())
        self.assertEqual([], self.remote_rows())
        self.assertTrue(batch.exists(), "the series batch stays spooled for the retry")
        # The refused series commit is named by a durable recovery ref before
        # the writer moved to published main to publish the quarantine.
        [recovery] = git(writer, "for-each-ref", "--format=%(refname)",
                         "refs/heads/recovery/").splitlines()
        refused = recovery.removeprefix("refs/heads/recovery/cell-series-")
        self.assertIn("series/hermit/fixture/2026-09.jsonl",
                      git(writer, "show", "--name-only", "--format=", refused))
        hook.unlink()
        retried = self.publish(publish_only=True)
        self.assertEqual([row("run-push-refused")], self.remote_rows())
        self.assertEqual(0, retried["raw_publication"]["quarantine"]["published"])
        self.assertFalse(batch.exists())

    def test_scheduled_backlog_publishes_the_quarantine_past_a_refused_series_batch(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )
        native_run = subprocess.run
        commands = []

        def sweep_child(command, **kwargs):
            if str(MODULE) in command:
                commands.append(command)
                command = bounded_fixture_command(
                    [sys.executable, "-B", "-c", bootstrap, *command[1:]])
            return native_run(command, **kwargs)

        bogus = self.spool_refused_series_batch()
        record = self.quarantine("run-behind-bogus", b"not json")
        series.append_quarantine(self.parent, "run-behind-bogus", [record])
        self.assertEqual({"series", "series-quarantine"},
                         set(publish_backlog.pending_stores(self.parent)))
        with mock.patch.object(publish_backlog, "STATE_ROOT", self.parent), \
             mock.patch.object(publish_backlog.subprocess, "run", side_effect=sweep_child):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                failed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
        sweep = json.loads(output.getvalue())
        reports = {report["store"]: report for report in sweep["reports"]}
        # The series store still fails, loudly, on its refused batch ...
        self.assertEqual(1, failed)
        self.assertEqual(1, sweep["failed_stores"])
        self.assertEqual(1, reports["series"]["rc"])
        self.assertEqual("failed", reports["series"]["state"])
        self.assertIn("month must be YYYY-MM", reports["series"]["error"])
        self.assertTrue(bogus.exists())
        # ... yet its own failure report shows the refused input published
        # after the refused batch, not stranded behind it ...
        self.assertEqual(1, reports["series"]["raw_publication"]["quarantine"]["published"])
        # ... and the quarantine store, run through the quarantine-only mode
        # that never reads the series spool, succeeds with nothing left over.
        self.assertEqual((0, 0), (reports["series-quarantine"]["rc"],
                                  reports["series-quarantine"]["published"]))
        self.assertIn("--quarantine-only", commands[-1])
        self.assertNotIn("--quarantine-only", commands[0])
        self.assertEqual([series.portable_quarantine_record(record)], self.remote_quarantine())
        self.assertEqual({"series": series.spool_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))

    def test_quarantine_only_publication_never_publishes_series_rows(self):
        batch = self.append("row-must-wait")
        record = self.quarantine("run-q-only", b"not json")
        series.append_quarantine(self.parent, "run-q-only", [record])
        report = self.publish(publish_only=True, quarantine_only=True)
        self.assertEqual("quarantine only", report["raw_publication"]["skipped"])
        self.assertEqual(1, report["raw_publication"]["quarantine"]["published"])
        self.assertEqual([], self.remote_rows())
        self.assertTrue(batch.exists())
        with self.assertRaisesRegex(mirror.MirrorRefused, "requires --publish-only"):
            self.publish(quarantine_only=True)

    def test_a_failure_before_publication_still_reports_the_runs_local_count(self):
        record = self.quarantine("run-early", b"not json")
        series.append_quarantine(self.parent, "run-early", [record])
        failure = mirror.failure_report(
            self.parent, mirror.MirrorRefused("another ledger publisher still holds the lock"),
            run_id="run-early", count=True)
        quarantine = failure["raw_publication"]["quarantine"]
        self.assertEqual("failed", failure["state"])
        self.assertTrue(quarantine["error"].startswith("not attempted: MirrorRefused"), quarantine)
        self.assertEqual({"run_id": "run-early", "published": None, "withheld": None,
                          "unpublished": 1, "records": 1}, quarantine["run"])
        self.assertEqual(1, quarantine["pending_records"])
        # No count is invented for a caller that did not ask for one.
        self.assertIsNone(mirror.failure_report(
            self.parent, mirror.MirrorRefused("x"), run_id=None, count=False))

    def test_missing_finalized_run_identity_preserves_verdict_and_pending_spool(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        import start_unit
        import run_registry

        spool = self.append("unbound-finalized")
        original = spool.read_bytes()
        for unit in (None, "", " "):
            with self.subTest(unit=unit):
                record_path = self.parent / "missing-unit.json"
                record = {"schema_version": run_registry.SCHEMA_VERSION,
                          "repo": "rrnewton/hermit", "checkout": str(self.source),
                          "target": self.target, "unit": unit, "canonical_verdict": "FAILED",
                          "wrapper_exit_code": start_unit.EXIT_FAILED,
                          "state": "completed", "e2e_result_root": str(self.root / "results")}
                run_registry.write_record(record_path, record)
                child = mock.Mock(side_effect=AssertionError("unbound publication must not launch"))
                result = start_unit.finalize_ledger_mirror(
                    self.parent, mirror.TOOL_ROOT, record_path, record=record, run=child)
                self.assertEqual("failed", result["ledger_mirror"]["state"])
                self.assertIn("canonical run unit", result["ledger_mirror"]["detail"])
                self.assertEqual("FAILED", result["canonical_verdict"])
                self.assertEqual(start_unit.EXIT_FAILED, result["wrapper_exit_code"])
                self.assertEqual(original, spool.read_bytes())
                child.assert_not_called()

    def test_finalized_run_uses_held_canonical_row_and_published_snapshot(self):
        self.append("finalized-cell")
        row_bytes = json.dumps({"run_id": "run-1", "record_id": "record-1", "schema_version": 10,
                                "started_at": "2026-09-21T12:00:00Z",
                                "finished_at": "2026-09-21T12:10:00Z"}).encode()
        calls = []
        original = mirror._run
        writer = self.parent / mirror.WRITER_RELATIVE

        def inspect(command, **kwargs):
            if (command[0] == "rust-script" and str(self.tool / "ci/compat-envelope/scorecard.rs") in command
                    and command[-1] != "--help"):
                calls.append(command)
                # Measured code gets a throwaway ledger copy, no routes and no credentials.
                ledger = Path(kwargs["environment"][series.LEDGER_ROOT_ENV])
                self.assertNotEqual(writer.resolve(), ledger.resolve())
                self.assertTrue(ledger.parent.name.startswith(mirror.RENDER_COPY_PREFIX), ledger)
                self.assertEqual(writer.parent, ledger.parent.parent)
                self.assertEqual("true", kwargs["environment"]["CARGO_NET_OFFLINE"])
                self.assertEqual(mirror.RENDERER_WITHHELD_ENV, kwargs["withhold"])
                self.assertEqual(command[:5], [
                    "rust-script", "--force", "--toolchain",
                    "nightly-2026-07-12-x86_64-unknown-linux-gnu",
                    str(self.tool / "ci/compat-envelope/scorecard.rs"),
                ])
                self.assertEqual("project-and-observe-results", command[5])
                row_path = Path(command[command.index("--finalized-row") + 1])
                snapshot_path = Path(command[command.index("--snapshot") + 1])
                self.assertEqual(row_bytes, row_path.read_bytes())
                self.assertEqual(mirror.hashlib.sha256(row_bytes).hexdigest(),
                                 command[command.index("--finalized-row-sha256") + 1])
                snapshot = json.loads(snapshot_path.read_bytes())
                self.assertEqual(mirror.REMOTE_URL, snapshot["source"]["repository"])
                self.assertEqual(["finalized-cell"], [row["event_id"] for row in snapshot["rows"]])
                self.assertEqual(series.SCORECARD_SNAPSHOT_SCHEMA, snapshot["schema"])
                self.assertEqual(series.SCORECARD_SNAPSHOT_SOURCE, snapshot["source"]["path"])
                self.assertEqual(1, snapshot["source"]["published_rows"])
                self.assertEqual([], snapshot["source"]["local_sources"])
                encoded = snapshot_path.read_bytes()
                digest = mirror.hashlib.sha256(encoded).hexdigest()
                self.assertEqual(snapshot, series._validate_snapshot_payload(encoded, expected_sha256=digest))
                for field in ("schema", "path", "published_rows"):
                    malformed = json.loads(encoded)
                    del (malformed if field == "schema" else malformed["source"])[field]
                    bad = series._snapshot_bytes(malformed)
                    with self.subTest(missing_wire_field=field), self.assertRaises(series.SeriesError):
                        series._validate_snapshot_payload(bad, expected_sha256=mirror.hashlib.sha256(bad).hexdigest())
                with self.assertRaisesRegex(series.SeriesError, "digest mismatch"):
                    series._validate_snapshot_payload(encoded, expected_sha256="0" * 64)
                self.assertEqual(str(self.parent), command[command.index("--state-root") + 1])
            return original(command, **kwargs)

        # Portable proof has its own real-publisher coverage (portable_fixture);
        # this test binds the held row and the published snapshot.
        with mock.patch.object(mirror, "finalized_producer_row", return_value=row_bytes) as query, \
                mock.patch.object(mirror, "prepare_portable_proof", return_value=None) as portable, \
                mock.patch.object(mirror, "_run", side_effect=inspect):
            report = mirror.mirror(self.parent, self.source, self.target, mirror.REMOTE_URL,
                                   net_wrapper="/usr/bin/env", tool_checkout=self.tool,
                                   results=self.root / "results", finalized_run_id="run-1")
        portable.assert_called_once_with(self.parent, row_bytes, net_wrapper="/usr/bin/env",
                                         attempts=mirror.DEFAULT_ATTEMPTS)
        self.assertEqual("published", report["state"])
        self.assertEqual(2, query.call_count)
        self.assertEqual(1, len(calls))
        self.assertFalse(Path(calls[0][calls[0].index("--finalized-row") + 1]).exists())
        self.assertEqual([], list(writer.parent.glob(mirror.RENDER_COPY_PREFIX + "*")))

    def test_canonical_run_change_refuses_derived_commit_without_losing_rows(self):
        self.append("finalized-cell")
        before = git(self.remote, "rev-parse", "main:scorecard/cells.json")
        rows = [json.dumps({"started_at": "2026-09-21T12:00:00Z", "finished_at": "2026-09-21T12:10:00Z",
                            "schema_version": 10, "record_id": record}).encode()
                for record in ["original", "correction"]]
        with mock.patch.object(mirror, "finalized_producer_row", side_effect=rows), \
                mock.patch.object(mirror, "prepare_portable_proof", return_value=None) as portable:
            report = mirror.mirror(self.parent, self.source, self.target, mirror.REMOTE_URL,
                                   net_wrapper="/usr/bin/env", tool_checkout=self.tool,
                                   results=self.root / "results", finalized_run_id="run-1")
        portable.assert_called_once_with(self.parent, rows[0], net_wrapper="/usr/bin/env",
                                         attempts=mirror.DEFAULT_ATTEMPTS)
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("changed during", report["scorecard"]["error"])
        self.assertEqual(before, git(self.remote, "rev-parse", "main:scorecard/cells.json"))
        self.assertEqual(["finalized-cell"], [row["event_id"] for row in self.remote_rows()])
        # The renderer ran in a scratch copy; a refused render copies nothing
        # into the writer and leaves no copy behind.
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("", git(writer, "status", "--porcelain"))
        self.assertEqual([], list(writer.parent.glob(mirror.RENDER_COPY_PREFIX + "*")))

    def portable_fixture(self):
        # The fast suite models canonical selection and the native writer. Exact
        # transport and real Git are exercised here; this is not schema-10
        # native or cross-host authority qualification.
        fixtures = mirror.TOOL_ROOT / "ci-hub/validate/tests/fixtures/schema10"
        selected = json.loads((fixtures / "ordinary-only-row.json").read_bytes())
        selected.update(repo="hermit", record_id="portable-fixture", commit=self.target,
                        cwd=str(self.source.resolve()), started_at="2026-09-21T12:00:00Z",
                        finished_at="2026-09-21T12:10:00Z")
        for role, ref in mirror.finalized_proof.row_references(selected).items():
            path = self.parent / ref["path"]
            path.parent.mkdir(parents=True, exist_ok=True)
            suffix = "plan.json" if role == "plan" else role + ".jsonl"
            path.write_bytes((fixtures / ("ordinary-only-" + suffix)).read_bytes())
        source = self.parent / "ignored/ci-hub/ledger-publisher-clone"
        if not source.exists():  # a test may prepare several terminals
            source.mkdir(parents=True)
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            git(source, "remote", "add", "origin", mirror.finalized_proof.PARENT_REPOSITORY)
        selection = {
            "row": selected,
            "terminal": {"repository": "rrnewton/hermit", "run_id": selected["run_id"],
                         "record_id": selected["record_id"], "event_id": "portable-terminal",
                         "event_sha256": "1" * 64, "hermit_commit": self.target},
            "parent": {"repository": mirror.finalized_proof.PARENT_REPOSITORY,
                       "commit": "2" * 40, "shard": "ledger/hermit/fixture/2026-09.jsonl",
                       "blob_oid": "3" * 40, "event_line_sha256": "4" * 64},
        }
        return mirror._row_bytes(selected), selection

    def test_portable_parent_source_binding_refuses_dirty_transitive_inputs(self):
        inputs = ("ci-hub/qualifying_receipt.py", "ci-hub/admission_paths.py",
                  "ci-hub/validate/gate_completeness.py", "ci-hub/validate/flaky-cells.json",
                  "ci-hub/health/ledger_spool_backlog.py", "scripts/check_commit_attribution.py",
                  "ci-hub/validate/retained-evidence-quarantine.json")
        for relative in inputs:
            path = self.parent / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("committed source fixture\\n")
        git(self.parent, "add", "--", *inputs)
        git(self.parent, "commit", "-qm", "synthetic source closure")
        head = git(self.parent, "rev-parse", "HEAD")
        with mock.patch.object(mirror, "TOOL_ROOT", self.parent):
            self.assertEqual(head, mirror._parent_tool_commit())
            for relative in inputs:
                path = self.parent / relative
                before = path.read_bytes()
                path.write_bytes(before + b"dirty source fixture\\n")
                with self.subTest(relative=relative), self.assertRaisesRegex(mirror.MirrorRefused, "uncommitted"):
                    mirror._parent_tool_commit()
                path.write_bytes(before)
            # Source identity does not impose whole-repository cleanliness.
            (self.parent / "unrelated-note").write_text("unrelated fixture\\n")
            self.assertEqual(head, mirror._parent_tool_commit())

    def test_portable_source_binding_ignores_a_parent_move_that_leaves_its_inputs(self):
        # Run from the shared checkout, the parent's HEAD moves with every push
        # to main while a schema-5 carrier is prepared (356-643 s). Comparing
        # HEAD refused validate ec16e766's carrier on every sweep on 2026-10-11
        # with "portable proof producer source changed during publication".
        inputs = ("ci-hub/qualifying_receipt.py", "ci-hub/admission_paths.py")
        for relative in inputs:
            path = self.parent / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("committed source fixture\n")
        git(self.parent, "add", "--", *inputs)
        git(self.parent, "commit", "-qm", "synthetic source closure")
        prepared = git(self.parent, "rev-parse", "HEAD")
        with mock.patch.object(mirror, "TOOL_ROOT", self.parent):
            self.assertTrue(mirror._parent_tool_source_unchanged(prepared))
            # A ledger append elsewhere in the parent moves HEAD only.
            (self.parent / "ledger-append.jsonl").write_text("{}\n")
            git(self.parent, "add", "--", "ledger-append.jsonl")
            git(self.parent, "commit", "-qm", "ledger: append 1 event(s)")
            self.assertNotEqual(prepared, git(self.parent, "rev-parse", "HEAD"))
            self.assertTrue(mirror._parent_tool_source_unchanged(prepared))
            # A committed change to a producer input is still refused.
            (self.parent / inputs[0]).write_text("changed source fixture\n")
            git(self.parent, "commit", "-qam", "change a producer input")
            self.assertFalse(mirror._parent_tool_source_unchanged(prepared))

    def test_portable_proof_follows_parent_then_cell_lock_and_same_derived_commit(self):
        encoded, selection = self.portable_fixture()
        self.append("portable-cell")
        order = []
        actual_publish, actual_writer = mirror.publisher.publish, mirror.publication_writer
        import validate_rows
        namespace = mock.MagicMock(spec=validate_rows.CommittedValidationNamespace)

        def publish(*args, **kwargs):
            if kwargs.get("store") == "ledger":
                order.append("parent-publish")
                self.assertEqual(self.parent, kwargs["source_root"])
                self.assertEqual(mirror.TOOL_ROOT / "scripts/parent-main-write", kwargs["serialized_writer"])
                return {"published": 1}
            # The refused-input spool is published after the series rows, in
            # the same held cell lock; it is recorded separately so the order
            # of both is asserted rather than counted as one.
            order.append("quarantine-publish" if kwargs.get("store") == "series-quarantine"
                         else "raw-publish")
            return actual_publish(*args, **kwargs)

        @contextlib.contextmanager
        def writer(*args, **kwargs):
            order.append("cell-lock")
            with actual_writer(*args, **kwargs) as held:
                yield held

        finished_with_rows = []
        actual_finish = mirror.finish_portable_proof

        def finish(prepared, parent, writer, snapshot, *args, **kwargs):
            finished_with_rows.append(snapshot["rows"])
            return actual_finish(prepared, parent, writer, snapshot, *args, **kwargs)

        with mock.patch.object(mirror, "finalized_producer_row", return_value=encoded) as live, \
                mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_select_published_terminal", return_value=(namespace, selection)) as committed, \
                mock.patch.object(mirror.publisher, "publish", side_effect=publish), \
                mock.patch.object(mirror, "publication_writer", side_effect=writer), \
                mock.patch.object(mirror, "finish_portable_proof", side_effect=finish):
            report = self.publish(results=self.root / "results", finalized_run_id=selection["row"]["run_id"])
        self.assertEqual("published", report["state"], report)
        self.assertEqual(["parent-publish", "cell-lock", "raw-publish", "quarantine-publish"], order)
        self.assertEqual(2, live.call_count)
        self.assertEqual(2, committed.call_count)
        # The preparation does not hand its namespace (the whole validation
        # ledger) forward, only its seed, which holds none of the ledger's
        # bytes; the recheck advances that to current main.
        self.assertIsNone(committed.call_args_list[0].kwargs.get("cache"))
        self.assertNotIn("namespace", committed.call_args_list[1].kwargs["cache"])
        self.assertIs(namespace.advance_seed.return_value,
                      committed.call_args_list[1].kwargs["cache"]["namespace_seed"])
        # The proof is staged after the rendered scorecard has used the rows,
        # and without them: every series row is about 7 GB of Python objects
        # (https://github.com/rrnewton/dev-hermit/issues/554).
        self.assertEqual([None], finished_with_rows)
        artifacts, _ = mirror.finalized_proof.capture_artifacts(self.parent, selection["row"])
        manifest = mirror.finalized_proof.make_manifest(selection, artifacts, parent_commit="8" * 40,
                                                       hermit_tool_commit=git(self.tool, "rev-parse", "HEAD"),
                                                       snapshot=mirror._published_snapshot(self.parent / mirror.WRITER_RELATIVE))
        path = mirror.finalized_proof.carrier_path(manifest)
        public = json.loads(git(self.remote, "show", "main:" + path))
        self.assertEqual(selection["terminal"], public["terminal"])
        self.assertEqual(artifacts, public["artifacts"])
        self.assertIn(path, report["scorecard"]["files"])
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        self.assertNotIn("row", public)

    def test_parent_proof_unavailable_preserves_raw_without_derived_credit(self):
        encoded, selection = self.portable_fixture()
        self.append("unavailable-parent")
        before = git(self.remote, "rev-parse", "main:scorecard/cells.json")
        with mock.patch.object(mirror, "finalized_producer_row", return_value=encoded), \
                mock.patch.object(mirror, "prepare_portable_proof", side_effect=mirror.MirrorRefused("canonical terminal unpublished")), \
                mock.patch.object(mirror, "build_scorecard") as native:
            report = self.publish(results=self.root / "results", finalized_run_id=selection["row"]["run_id"])
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("unpublished", report["scorecard"]["error"])
        native.assert_not_called()
        self.assertEqual(before, git(self.remote, "rev-parse", "main:scorecard/cells.json"))
        self.assertEqual(["unavailable-parent"], [r["event_id"] for r in self.remote_rows()])
        self.assertEqual("", git(self.remote, "ls-tree", "-r", "main", "--", "scorecard/finalized"))

    def test_non_cargo_release_payload_never_reaches_the_scorecard(self):
        # A Buck full run observes the release E2E payload. Its cells must not
        # become scorecard observations; raw rows still publish unchanged. A row
        # whose builder and payload disagree names no builder and is refused too.
        cases = json.loads((Path(__file__).resolve().parents[2] / "validate" / "tests"
                            / "fixtures" / "release-payload-cases.json").read_text())["cases"]
        self.assertEqual(11, sum(case["expected"] is not None for case in cases))
        for index, case in enumerate(cases):
            encoded = json.dumps(_apply_case({"schema_version": 10, "started_at": "2026-09-21T12:00:00Z",
                                              "finished_at": "2026-09-21T12:10:00Z"}, case)).encode()
            if case["expected"] != "cargo":
                with self.subTest(case=case["label"]):
                    event = f"payload-{index}"
                    self.append(event)
                    before = git(self.remote, "rev-parse", "main:scorecard/cells.json")
                    with mock.patch.object(mirror, "finalized_producer_row", return_value=encoded), \
                            mock.patch.object(mirror, "prepare_portable_proof") as portable, \
                            mock.patch.object(mirror, "build_scorecard") as native:
                        report = self.publish(results=self.root / "results", finalized_run_id=event)
                    self.assertEqual("derived-failed", report["state"])
                    self.assertIn("non-Cargo release payload", report["scorecard"]["error"])
                    portable.assert_not_called()
                    native.assert_not_called()
                    self.assertEqual(before, git(self.remote, "rev-parse", "main:scorecard/cells.json"))
                    self.assertIn(event, [r["event_id"] for r in self.remote_rows()])
                continue
            # Direction control: a pairless row dated before the cutoff, or the
            # exact Cargo pair, proceeds.
            with self.subTest(case=case["label"]):
                with mock.patch.object(mirror, "finalized_producer_row", return_value=encoded), \
                        mock.patch.object(mirror, "prepare_portable_proof", return_value=None), \
                        mock.patch.object(mirror, "build_scorecard",
                                          side_effect=mirror.MirrorRefused("reached scorecard")) as native:
                    report = self.publish(results=self.root / "results", finalized_run_id=f"cargo-{index}")
                native.assert_called_once()
                self.assertEqual("reached scorecard", report["scorecard"]["error"])

    def schema10_prepared(self, **row_updates):
        """A prepared schema-10 proof: its operands are the run's own retained
        plan, cell and test artifacts, captured exactly as prepare_portable_proof
        captures them. No Hermit tool derives them."""
        _encoded, selection = self.portable_fixture()
        selection["row"].update(row_updates)
        artifacts, objects = mirror.finalized_proof.capture_artifacts(self.parent, selection["row"])
        return {"selection": selection, "source": self.parent, "artifacts": artifacts,
                "objects": objects, "parent_tool_commit": "8" * 40, "derived": False}

    def schema10_proof_doubles(self, prepared):
        selected = prepared["selection"]
        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(mirror, "finalized_producer_row",
                                              return_value=mirror._row_bytes(selected["row"])))
        stack.enter_context(mock.patch.object(mirror, "prepare_portable_proof", return_value=prepared))
        stack.enter_context(mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40))
        stack.enter_context(mock.patch.object(mirror, "_select_published_terminal",
                                              return_value=(mock.MagicMock(), selected)))
        stack.enter_context(mock.patch.object(
            mirror, "emit_derived_portable_proof",
            side_effect=AssertionError("schema-10 operands are captured, never derived")))
        return stack

    def derived_fixture(self, **row_updates):
        _encoded, selection = self.portable_fixture()
        selected = selection["row"]
        selected.update(schema_version=5, cell_results=None, result="fail", validation_complete=False)
        # Row facts are part of the derived binding, so apply them before it.
        selected.update(row_updates)
        if "run_id" in row_updates:
            selection["terminal"]["run_id"] = row_updates["run_id"]
        cell = {"lane": "portable", "category": "c-programs", "test": "fixture",
                "mode": "verify", "backend": "ptrace"}
        selected["raw_result_input_census_v1"] = {
            "schema": 1, "run_id": selected["run_id"], "hermit_sha": selected["commit"],
            "files": [{"path": "portable/results.jsonl", "bytes": 20, "sha256": "a" * 64,
                       "rows": [{"line": 1, "cell": cell, "attempt": 1}]}],
        }
        value = {"schema": 1, "authority": mirror.finalized_proof.DERIVED_AUTHORITY,
                 **mirror.finalized_proof.derived_row_binding(selected), "attempts": [{
                     "input_path": "portable/results.jsonl", "input_line": 1, "cell": cell,
                     "attempt": 1, "source_row_sha256": "b" * 64, "outcome": "FAIL",
                     "result": None, "failure_class": None, "ordinary_verdict": {}, "backend_parity": None}]}
        encoded = mirror.finalized_proof.canonical_bytes(value) + b"\n"
        prepared = {"selection": selection, "source": self.parent, "artifacts": None,
                    "objects": None, "parent_tool_commit": "8" * 40, "derived": True}
        return prepared, encoded

    def test_native_derived_emission_is_explicit_and_failed_child_cannot_stage(self):
        prepared, encoded = self.derived_fixture()
        row_before = mirror._row_bytes(prepared["selection"]["row"])
        calls = []

        def native(command, **kwargs):
            calls.append(command)
            self.assertEqual("compatibility-website-derived-census", command[5])
            self.assertEqual(str(mirror.TOOL_ROOT / "ci-hub/ci-hub.rs"), command[4])
            self.assertEqual(str(self.parent / "target"), kwargs["environment"]["CARGO_TARGET_DIR"])
            self.assertEqual(row_before, Path(command[command.index("--row-json") + 1]).read_bytes())
            Path(command[command.index("--output-json") + 1]).write_bytes(encoded)
            return subprocess.CompletedProcess(command, 0, "", "")

        with mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_committed_tool_source", return_value="9" * 40), \
                mock.patch.object(mirror.rust_toolchain, "selected_toolchain", return_value="fixture-toolchain"), \
                mock.patch.object(mirror, "_network", side_effect=native):
            self.assertEqual("9" * 40, mirror.emit_derived_portable_proof(
                prepared, self.parent, self.root / "results", self.tool, net_wrapper="fixture"))
        self.assertEqual(1, len(calls))
        self.assertEqual(2, prepared["artifacts"]["schema"])
        self.assertEqual(row_before, mirror._row_bytes(prepared["selection"]["row"]))
        for status in (126, 127):
            prepared.update(artifacts=None, objects=None)
            with mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                    mock.patch.object(mirror, "_committed_tool_source", return_value="9" * 40), \
                    mock.patch.object(mirror.rust_toolchain, "selected_toolchain", return_value="fixture-toolchain"), \
                    mock.patch.object(mirror, "_network", side_effect=mirror.MirrorRefused(f"native exit {status}")), \
                    self.assertRaises(mirror.MirrorRefused):
                mirror.emit_derived_portable_proof(prepared, self.parent, self.root / "results", self.tool,
                                                    net_wrapper="fixture")
            self.assertIsNone(prepared["artifacts"])

    def test_native_derived_emission_preserves_explicit_existing_target(self):
        prepared, encoded = self.derived_fixture()
        target = self.root / "existing-target"
        target.mkdir()
        calls = []

        def native(command, **kwargs):
            calls.append(command)
            self.assertEqual(str(target), kwargs["environment"]["CARGO_TARGET_DIR"])
            self.assertEqual(str(mirror.TOOL_ROOT / "ci-hub/ci-hub.rs"), command[4])
            self.assertEqual("compatibility-website-derived-census", command[5])
            Path(command[command.index("--output-json") + 1]).write_bytes(encoded)
            return subprocess.CompletedProcess(command, 0, "", "")

        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target)}), \
                mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_committed_tool_source", return_value="9" * 40), \
                mock.patch.object(mirror.rust_toolchain, "selected_toolchain", return_value="fixture-toolchain"), \
                mock.patch.object(mirror, "_network", side_effect=native):
            self.assertEqual("9" * 40, mirror.emit_derived_portable_proof(
                prepared, self.parent, self.root / "results", self.tool, net_wrapper="fixture"))
        self.assertEqual(1, len(calls))
        self.assertEqual(2, prepared["artifacts"]["schema"])
        self.assertFalse((self.parent / "target").exists())

    def derived_census_program(self, encoded: bytes) -> Path:
        """A stored ci-hub double: checks rust-script's variables, then writes ENCODED."""
        payload = self.root / "derived-payload.json"
        payload.write_bytes(encoded)
        program = self.root / "stored-ci-hub"
        program.write_text(
            f"#!{sys.executable}\nimport os, shutil, sys\n"
            f"assert os.environ['RUST_SCRIPT_PATH'] == {str(mirror.TOOL_ROOT / 'ci-hub/ci-hub.rs')!r}\n"
            "assert os.environ['RUSTUP_TOOLCHAIN'] == 'fixture-toolchain'\n"
            "assert sys.argv[1] == 'compatibility-website-derived-census', sys.argv\n"
            f"shutil.copyfile({str(payload)!r}, sys.argv[sys.argv.index('--output-json') + 1])\n")
        program.chmod(0o755)
        return program

    def test_native_derived_emission_runs_the_stored_ci_hub_program(self):
        prepared, encoded = self.derived_fixture()
        program = self.derived_census_program(encoded)
        script = mirror.TOOL_ROOT / "ci-hub/ci-hub.rs"
        built = mirror.rust_tool_cache.Built(
            family="ci-hub", tool=mirror.TOOL_ROOT, compiler="fixture-toolchain", slot=self.root,
            key="c" * 64, outcome="hit", programs={"ci-hub": program},
            scripts={"ci-hub": (script, "ci-hub")})
        sessions = []

        def prepare(family, state_root, tool, compiler, *, net_wrapper):
            sessions.append(mirror.rust_tool_cache.active())
            self.assertEqual((mirror.rust_tool_cache.CI_HUB, self.parent, mirror.TOOL_ROOT,
                              "fixture-toolchain", "fixture"),
                             (family, state_root, tool, compiler, net_wrapper))
            return built

        self.direct_path.stop()
        with mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_committed_tool_source", return_value="9" * 40), \
                mock.patch.object(mirror.rust_toolchain, "selected_toolchain", return_value="fixture-toolchain"), \
                mock.patch.object(mirror.rust_tool_cache, "prepare", side_effect=prepare), \
                mock.patch.object(mirror, "_network") as network:
            self.assertEqual("9" * 40, mirror.emit_derived_portable_proof(
                prepared, self.parent, self.root / "results", self.tool, net_wrapper="fixture"))
        network.assert_not_called()
        self.assertEqual([True], sessions)
        self.assertFalse(mirror.rust_tool_cache.active())
        self.assertEqual(2, prepared["artifacts"]["schema"])
        self.assertFalse((self.parent / "target").exists())

    def test_native_derived_emission_builds_directly_when_the_cache_fails(self):
        prepared, encoded = self.derived_fixture()
        calls = []

        def native(command, **kwargs):
            calls.append(command)
            self.assertEqual(["rust-script", "--force", "--toolchain", "fixture-toolchain",
                              str(mirror.TOOL_ROOT / "ci-hub/ci-hub.rs"),
                              "compatibility-website-derived-census"], command[:6])
            self.assertEqual(str(self.parent / "target"), kwargs["environment"]["CARGO_TARGET_DIR"])
            Path(command[command.index("--output-json") + 1]).write_bytes(encoded)
            return subprocess.CompletedProcess(command, 0, "", "")

        self.direct_path.stop()
        stderr = io.StringIO()
        with mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_committed_tool_source", return_value="9" * 40), \
                mock.patch.object(mirror.rust_toolchain, "selected_toolchain", return_value="fixture-toolchain"), \
                mock.patch.object(mirror.rust_tool_cache, "prepare",
                                  side_effect=OSError(errno.ENOSPC, "fixture: no space")), \
                mock.patch.object(mirror, "_network", side_effect=native), \
                contextlib.redirect_stderr(stderr):
            self.assertEqual("9" * 40, mirror.emit_derived_portable_proof(
                prepared, self.parent, self.root / "results", self.tool, net_wrapper="fixture"))
        self.assertEqual(1, len(calls))
        self.assertIn("ci-hub tool cache unavailable", stderr.getvalue())
        self.assertIn("fixture: no space", stderr.getvalue())
        self.assertEqual(2, prepared["artifacts"]["schema"])

    def test_proof_only_attachment_preserves_published_projection_and_requires_new_operands(self):
        # Native/parent authorities are explicit doubles. This tests real Git
        # delivery and preservation, not the native comparison verifier.
        self.append("existing-finalized-cell")
        original = self.publish()
        self.assertEqual("published", original["state"], original)
        before = self.published_projection()
        prepared, encoded = self.derived_fixture()
        selected = prepared["selection"]
        native_calls = []

        def native(value, *args, **kwargs):
            native_calls.append(value)
            value["artifacts"], value["objects"] = mirror.finalized_proof.capture_derived_cells(selected["row"], encoded)
            return git(self.tool, "rev-parse", "HEAD")

        with mock.patch.object(mirror, "finalized_producer_row", return_value=mirror._row_bytes(selected["row"])), \
                mock.patch.object(mirror, "prepare_portable_proof", return_value=prepared), \
                mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_select_published_terminal", return_value=(mock.MagicMock(), selected)), \
                mock.patch.object(mirror, "emit_derived_portable_proof", side_effect=native), \
                mock.patch.object(mirror, "build_scorecard") as scorecard:
            report = self.publish(results=self.root / "results", finalized_run_id=selected["row"]["run_id"],
                                  portable_proof_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertIn("portable_proof", report)
        self.assertNotIn("scorecard", report)
        self.assertEqual(1, len(native_calls))
        scorecard.assert_not_called()
        self.assertEqual(before, self.published_projection())
        self.assertEqual(original["commit"], report["existing_scorecard"]["commit"])
        self.assertEqual(["existing-finalized-cell"], [value["event_id"] for value in self.remote_rows()])
        self.assertEqual("fail", selected["row"]["result"])
        writer = self.parent / mirror.WRITER_RELATIVE
        snapshot = mirror._published_snapshot(writer)
        metadata = json.loads((writer / "latest.json").read_bytes())
        for field, value in (("source_tree", "0" * 40), ("rows_sha256", "0" * 64),
                             ("scorecard_hermit_commit", "0" * 40)):
            changed = dict(metadata, **{field: value})
            (writer / "latest.json").write_text(json.dumps(changed))
            with self.subTest(field=field), self.assertRaises(mirror.MirrorRefused):
                mirror.published_scorecard_binding(writer, snapshot, self.target)
        (writer / "latest.json").write_text(json.dumps(metadata))
        # The writer stores the bound history in parts; a changed document and
        # a part that no longer matches its manifest are both refused.
        stored = mirror.scorecard_cells.read_tree(writer)
        self.assertEqual(stored_history(self.remote), stored)
        self.assertFalse((writer / mirror.scorecard_cells.LOGICAL_PATH).exists())
        mirror.published_scorecard_binding(writer, snapshot, self.target)
        mirror.scorecard_cells.replace_tree(writer, mirror.scorecard_cells.split(b"changed"))
        with self.assertRaisesRegex(mirror.MirrorRefused, "bytes differ"):
            mirror.published_scorecard_binding(writer, snapshot, self.target)
        mirror.scorecard_cells.replace_tree(writer, mirror.scorecard_cells.split(stored))
        mirror.published_scorecard_binding(writer, snapshot, self.target)
        part = writer / mirror.scorecard_cells.part_path(0)
        part.write_bytes(part.read_bytes()[:-1] + b"X")
        with self.assertRaisesRegex(mirror.MirrorRefused, "part-0000 differs from its manifest entry"):
            mirror.published_scorecard_binding(writer, snapshot, self.target)

    def test_proof_only_attachment_after_raw_append_keeps_both_source_bindings(self):
        # As above, native authority is an explicit double; Git, the historical
        # source binding and the current-series carrier transaction are real.
        self.append("frozen-scorecard-cell")
        original = self.publish()
        self.assertEqual("published", original["state"], original)
        before = self.published_projection()
        self.append("later-raw-cell")
        appended = self.publish(publish_only=True)
        self.assertEqual("published", appended["state"], appended)
        self.assertNotEqual(original["commit"], appended["commit"])
        current_rows = self.remote_rows()
        self.assertEqual({"frozen-scorecard-cell", "later-raw-cell"},
                         {value["event_id"] for value in current_rows})
        prepared, encoded = self.derived_fixture()
        selected = prepared["selection"]

        def native(value, *args, **kwargs):
            value["artifacts"], value["objects"] = mirror.finalized_proof.capture_derived_cells(selected["row"], encoded)
            return git(self.tool, "rev-parse", "HEAD")

        with mock.patch.object(mirror, "finalized_producer_row", return_value=mirror._row_bytes(selected["row"])), \
                mock.patch.object(mirror, "prepare_portable_proof", return_value=prepared), \
                mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40), \
                mock.patch.object(mirror, "_select_published_terminal", return_value=(mock.MagicMock(), selected)), \
                mock.patch.object(mirror, "emit_derived_portable_proof", side_effect=native), \
                mock.patch.object(mirror, "build_scorecard") as scorecard:
            report = self.publish(results=self.root / "results", finalized_run_id=selected["row"]["run_id"],
                                  portable_proof_only=True)
        self.assertEqual("published", report["state"], report)
        scorecard.assert_not_called()
        self.assertEqual(before, self.published_projection())
        self.assertEqual(current_rows, self.remote_rows())
        self.assertEqual(appended["commit"], report["series_commit"])
        carrier_paths = [path for path in report["portable_proof"]["files"]
                         if path.startswith("scorecard/finalized/runs/")]
        self.assertEqual(1, len(carrier_paths))
        carrier = json.loads(git(self.remote, "show", "main:" + carrier_paths[0]))
        self.assertEqual(appended["commit"], carrier["series"]["commit"])
        self.assertEqual(selected["terminal"], carrier["terminal"])
        self.assertEqual(5, selected["row"]["schema_version"])
        self.assertEqual("fail", selected["row"]["result"])
        self.assertFalse(selected["row"]["validation_complete"])
        self.assertIsNone(selected["row"]["cell_results"])

    def test_historical_scorecard_refuses_unbound_source_and_changed_original_events(self):
        self.append("original-cell")
        self.assertEqual("published", self.publish()["state"])
        self.append("later-cell")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        snapshot = mirror._published_snapshot(writer)
        metadata_path = writer / "latest.json"
        encoded = metadata_path.read_bytes()
        metadata = json.loads(encoded)
        mirror.published_scorecard_binding(writer, snapshot, self.target)
        for field, value in (("source_tree", "0" * 40), ("rows_sha256", "0" * 64),
                             ("rows", metadata["rows"] + 1), ("series_commit", "0" * 40)):
            metadata_path.write_text(json.dumps(dict(metadata, **{field: value})))
            with self.subTest(field=field), self.assertRaises(mirror.MirrorRefused):
                mirror.published_scorecard_binding(writer, snapshot, self.target)
        # Even a real commit with identical source bytes is not authority when
        # it is outside the admitted publication's ancestry.
        orphan = git(writer, "commit-tree", git(writer, "rev-parse", metadata["source_commit"] + "^{tree}"),
                     "-m", "unrelated source with identical tree")
        metadata_path.write_text(json.dumps(dict(metadata, source_commit=orphan, series_commit=orphan)))
        with self.assertRaises(mirror.MirrorRefused):
            mirror.published_scorecard_binding(writer, snapshot, self.target)
        # Naming the current source must not silently rebind the old digest.
        metadata_path.write_text(json.dumps(dict(metadata, source_commit=snapshot["source"]["commit"],
                                                series_commit=snapshot["source"]["commit"])))
        with self.assertRaisesRegex(mirror.MirrorRefused, "immutable raw source"):
            mirror.published_scorecard_binding(writer, snapshot, self.target)
        metadata_path.write_bytes(encoded)
        # The history is stored in parts: store a valid split of other bytes.
        self.assertFalse((writer / mirror.scorecard_cells.LOGICAL_PATH).exists())
        scorecard_bytes = mirror.scorecard_cells.read_tree(writer)
        self.assertIsNotNone(scorecard_bytes)
        mirror.scorecard_cells.replace_tree(writer, mirror.scorecard_cells.split(scorecard_bytes + b" "))
        with self.assertRaisesRegex(mirror.MirrorRefused, "bytes differ"):
            mirror.published_scorecard_binding(writer, snapshot, self.target)
        git(writer, "checkout", "HEAD", "--", mirror.scorecard_cells.PARTS_DIRECTORY)
        git(writer, "clean", "-fq", "--", mirror.scorecard_cells.PARTS_DIRECTORY)
        self.assertEqual(scorecard_bytes, mirror.scorecard_cells.read_tree(writer))
        self.assertEqual("", git(writer, "status", "--porcelain"))
        shard = writer / "series/hermit/fixture/2026-09.jsonl"
        for original_rows in ([row("original-cell", "diverged")], []):
            shard.write_text("".join(json.dumps(value) + "\n" for value in [*original_rows, row("later-cell")]))
            git(writer, "add", "series")
            git(writer, "commit", "-qm", "invalid mutation of historical cohort")
            with self.subTest(original_rows=original_rows), \
                    self.assertRaisesRegex(mirror.MirrorRefused, "changed or removed"):
                mirror.published_scorecard_binding(writer, mirror._published_snapshot(writer), self.target)

    def test_historical_scorecard_preserves_json_types_in_original_events(self):
        original = dict(row("original-cell"), extra={"flag": True, "count": 1})
        series.append_cell_rows(self.parent, series.spool_dir(self.parent) / "original.jsonl", [original])
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        mirror.published_scorecard_binding(writer, mirror._published_snapshot(writer), self.target)
        shard = writer / "series/hermit/fixture/2026-09.jsonl"
        for extra in ({"flag": 1, "count": 1}, {"flag": True, "count": 1.0}):
            changed = dict(original, extra=extra)
            self.assertEqual(original, changed, "Python equality alone misses this mutation")
            self.assertNotEqual(series._canonical_row_json(original), series._canonical_row_json(changed))
            shard.write_text(json.dumps(changed) + "\n")
            git(writer, "add", "series")
            git(writer, "commit", "-qm", "invalid JSON type mutation of historical event")
            with self.subTest(extra=extra), self.assertRaisesRegex(mirror.MirrorRefused, "changed or removed"):
                mirror.published_scorecard_binding(writer, mirror._published_snapshot(writer), self.target)

    # --- refused series inputs (series quarantine) -------------------------

    QUARANTINE_SHARD = "quarantine/series/hermit/fixture/2026-09-21T12.jsonl"

    def quarantine(self, run_id: str, raw: bytes, *, emitted_at: str = "2026-09-21T12:00:00Z") -> dict:
        return series.quarantine_record(
            stage="projection", run_id=run_id, producer="validate", tree="a" * 40,
            emitted_at=emitted_at, error="MalformedRow", reason="fixture refusal",
            inputs=[series.quarantine_input(1, raw)], host="fixture",
        )

    def remote_quarantine_rows(self) -> list[dict]:
        """The rows published in QUARANTINE_SHARD, as stored."""
        shown = subprocess.run(["git", "-C", str(self.remote), "show", f"main:{self.QUARANTINE_SHARD}"],
                               capture_output=True, text=True, check=False)
        if shown.returncode != 0:
            return []
        return [json.loads(line) for line in shown.stdout.split("\n") if line.strip()]

    def remote_quarantine(self) -> list[dict]:
        """The records QUARANTINE_SHARD publishes: each v3 row expanded through
        the input files published beside it, so a missing or wrong file fails."""
        rows = self.remote_quarantine_rows()
        refs = [item["raw_ref"] for row in rows if row["schema"] == series.QUARANTINE_REF_SCHEMA
                for item in row["inputs"]]
        files = series._ledger_files(self.remote, "main", refs)
        return [series.expand_quarantine_record(row, files) for row in rows]

    def remote_input_files(self) -> list[str]:
        return git(self.remote, "ls-tree", "-r", "--name-only", "main", "--",
                   series.QUARANTINE_INPUTS_ROOT).splitlines()

    def refuse_quarantine_pushes(self) -> Path:
        hook = self.remote / "hooks/pre-receive"
        hook.write_text(
            "#!/bin/sh\nwhile read old new ref; do\n"
            " if git diff --name-only \"$old\" \"$new\" | grep -q '^quarantine/'; then\n"
            "  echo deliberate-quarantine-refusal >&2; exit 1\n fi\ndone\nexit 0\n"
        )
        hook.chmod(0o755)
        return hook

    def test_quarantine_publishes_beside_the_series_and_is_counted(self):
        needle = series._OWNER_PATH_NEEDLE
        plain = self.quarantine("run-q", b'{"schema": 4, "truncated')
        owner = self.quarantine("run-q", json.dumps({"cwd": needle + "alice/work"}).encode())
        self.append("run-q")
        retained = series.append_quarantine(self.parent, "run-q", [plain, owner])
        self.assertEqual(2, retained["written"])
        local = retained["path"]
        self.assertEqual(series.quarantine_dir(self.parent), local.parent)
        exact = local.read_bytes()

        report = self.publish(publish_only=True)
        quarantine = report["raw_publication"]["quarantine"]
        self.assertIsNone(quarantine["error"], quarantine)
        self.assertEqual(2, quarantine["published"])
        self.assertEqual(0, quarantine["pending_records"])
        self.assertEqual(0, quarantine["held_records"])
        self.assertEqual([row("run-q")], self.remote_rows(), "the series rows publish as before")
        published = self.remote_quarantine()
        expected = series.union_events([series.portable_quarantine_record(plain),
                                        series.portable_quarantine_record(owner)])
        self.assertEqual(expected, published)
        self.assertNotIn(needle, git(self.remote, "show", f"main:{self.QUARANTINE_SHARD}"))
        series.validate_quarantine_records(published, public=True)
        # Stored in the v3 form: each row names a file holding its input's
        # published bytes, and holds no bytes itself.
        rows = self.remote_quarantine_rows()
        self.assertEqual([series.QUARANTINE_REF_SCHEMA] * 2, [row["schema"] for row in rows])
        self.assertEqual([(None, None)] * 2, [(row["inputs"][0]["raw"], row["inputs"][0]["raw_base64"])
                                              for row in rows])
        portable = {record["event_id"]: record["inputs"][0]["raw"].encode() for record in expected}
        self.assertEqual(sorted(series.quarantine_input_path(hashlib.sha256(content).hexdigest())
                                for content in portable.values()),
                         self.remote_input_files())
        for stored in rows:
            ref = stored["inputs"][0]["raw_ref"]
            self.assertEqual(portable[stored["event_id"]], git_bytes(self.remote, "show", f"main:{ref}"))
            self.assertNotIn(needle.encode(), git_bytes(self.remote, "show", f"main:{ref}"))
        # Nothing is read as series data: the series tree is unchanged.
        self.assertEqual("series/hermit/fixture/2026-09.jsonl",
                         git(self.remote, "ls-tree", "-r", "--name-only", "main", "series"))
        # The exact local copy waits in published/ until the ledger reads it.
        self.assertFalse(local.exists())
        self.assertEqual(exact, (local.parent / "published" / local.name).read_bytes())

        writer = self.parent / mirror.WRITER_RELATIVE
        counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                            net_wrapper="/usr/bin/env", attempts=1, run_id="run-q")
        self.assertEqual({"run_id": "run-q", "published": 2, "withheld": 0, "unpublished": 0,
                          "records": 2}, counted["run"])
        self.assertFalse((local.parent / "published" / local.name).exists(), "reaped once readable")

        # A replay of the same refusals publishes nothing new.
        replay = [self.quarantine("run-q", b'{"schema": 4, "truncated', emitted_at="2026-10-02T12:00:00Z")]
        replayed = series.append_quarantine(self.parent, "run-q", replay)
        self.assertIsNone(replayed["published_check_error"])
        self.assertEqual((0, 1), (replayed["written"], replayed["duplicates"]))
        again = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((0, None), (again["published"], again["error"]))
        self.assertEqual(expected, self.remote_quarantine())

    def test_quarantine_shards_are_hourly_and_month_shards_stay_readable(self):
        # A month per host reached 257.7 MB and GitHub refused the push
        # (https://github.com/rrnewton/dev-hermit/issues/499).
        self.assertEqual("2026-09-21T13", series.quarantine_bucket("2026-09-21T13:05:59Z"))
        self.assertEqual("2026-09-21T13", series.quarantine_bucket("2026-09-21T13:05:59.25Z"))
        for bad in ("2026-09-21", "2026-09-21Tnoon", "2026-09-21T1:00Z", "", None):
            with self.subTest(emitted_at=bad), self.assertRaises(series.MalformedRow):
                series.quarantine_bucket(bad)
        # The record check agrees, so a record it accepts always has a shard.
        with self.assertRaises(series.MalformedQuarantine):
            self.quarantine("run-hourless", b"x", emitted_at="2026-09-21T1:00Z")
        self.assertEqual("quarantine/series/hermit/fixture/2026-09-21T13.jsonl",
                         series.quarantine_shard_path("hermit", "fixture", "2026-09-21T13"))
        self.assertEqual("quarantine/series/hermit/fixture/2026-09.jsonl",
                         series.quarantine_shard_path("hermit", "fixture", "2026-09"))
        for bucket in ("2026-09-21", "2026-09-21T1", "2026-09-21T13:05", "../x"):
            with self.subTest(bucket=bucket), self.assertRaises(series.MalformedRow):
                series.quarantine_shard_path("hermit", "fixture", bucket)
        for host in ("..", "a/b", ""):
            with self.subTest(host=host), self.assertRaises(series.MalformedRow):
                series.quarantine_shard_path("hermit", host, "2026-09-21T13")

    def test_a_month_shard_published_before_hourly_shards_is_never_appended_to(self):
        old = self.quarantine("run-m", b"published monthly")
        legacy = "quarantine/series/hermit/fixture/2026-09.jsonl"
        other = self.root / "monthly-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        (other / legacy).parent.mkdir(parents=True, exist_ok=True)
        (other / legacy).write_text(
            json.dumps(series.portable_quarantine_record(old), sort_keys=True) + "\n")
        git(other, "add", "quarantine")
        git(other, "commit", "-qm", "series-quarantine: a month shard")
        git(other, "push", "-q", "origin", "main")
        legacy_text = git(self.remote, "show", f"main:{legacy}")

        new = self.quarantine("run-h", b"published hourly", emitted_at="2026-09-21T13:05:00Z")
        with mock.patch.object(series, "published_quarantine_at",
                               side_effect=series.SeriesError("ledger unreadable")):
            retained = series.append_quarantine(self.parent, "run-h", [old, new])
        self.assertEqual(2, retained["written"])
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, 1, None), (report["published"], report["already_published"], report["error"]))
        self.assertEqual(legacy_text, git(self.remote, "show", f"main:{legacy}"))
        self.assertEqual(
            ["quarantine/series/hermit/fixture/2026-09-21T13.jsonl", legacy],
            git(self.remote, "ls-tree", "-r", "--name-only", "main", "quarantine/series").splitlines())
        self.assertEqual([series.quarantine_input_path(hashlib.sha256(b"published hourly").hexdigest())],
                         self.remote_input_files())
        _, published = series.published_quarantine_at_repo(self.remote, "main")
        self.assertEqual(
            sorted([old["event_id"], new["event_id"]]), sorted(r["event_id"] for r in published))

    def test_a_shard_over_the_ceiling_is_refused_before_its_push_and_the_spool_kept(self):
        record = self.quarantine("run-big", b"x" * 4096)
        local = series.append_quarantine(self.parent, "run-big", [record])["path"]
        exact = local.read_bytes()
        # The shard holds the v3 row; its input is in a file of its own.
        slim, _ = series.slim_quarantine_record(series.portable_quarantine_record(record))
        self.assertEqual(series.QUARANTINE_REF_SCHEMA, slim["schema"])
        size = len(json.dumps(slim, sort_keys=True)) + 1
        # Fail the push if one is attempted: the ceiling must refuse first.
        self.refuse_quarantine_pushes()
        with mock.patch.object(mirror.publisher, "QUARANTINE_SHARD_CEILING_BYTES", size - 1):
            report = self.publish(publish_only=True, attempts=3)["raw_publication"]["quarantine"]
        self.assertIn(
            f"{self.QUARANTINE_SHARD}: one series-quarantine row is ", report["error"])
        self.assertIn(
            f"over the {size - 1}-byte shard ceiling; spool retained", report["error"])
        self.assertNotIn("deliberate-quarantine-refusal", report["error"])
        self.assertEqual((0, 1), (report["published"], report["pending_records"]))
        self.assertEqual(exact, local.read_bytes())
        self.assertEqual([], self.remote_quarantine())
        (self.remote / "hooks/pre-receive").unlink()
        with mock.patch.object(mirror.publisher, "QUARANTINE_SHARD_CEILING_BYTES", size + 1024):
            retried = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (retried["published"], retried["error"]))

    def test_a_replayed_refusal_published_once_even_when_the_writer_could_not_dedupe(self):
        first = self.quarantine("run-d", b"not json")
        series.append_quarantine(self.parent, "run-d", [first])
        self.publish(publish_only=True)
        self.publish(publish_only=True)  # reaps the local published/ copy
        self.assertEqual([], list((series.quarantine_dir(self.parent) / "published").glob("*.jsonl")))
        later = self.quarantine("run-d", b"not json", emitted_at="2026-10-02T12:00:00Z")
        with mock.patch.object(series, "published_quarantine_at",
                               side_effect=series.SeriesError("ledger unreadable")):
            retained = series.append_quarantine(self.parent, "run-d", [later])
        self.assertEqual("ledger unreadable", retained["published_check_error"])
        self.assertEqual(1, retained["written"])
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((0, 1, None), (report["published"], report["already_published"], report["error"]))
        self.assertEqual([series.portable_quarantine_record(first)], self.remote_quarantine())
        self.assertFalse(git(self.remote, "ls-tree", "-r", "--name-only", "main",
                             "quarantine/series/hermit/fixture/2026-10-02T12.jsonl"))

    def test_quarantine_push_failure_is_recorded_and_the_spool_retried(self):
        self.append("run-f")
        record = self.quarantine("run-f", b"not json")
        local = series.append_quarantine(self.parent, "run-f", [record])["path"]
        exact = local.read_bytes()
        hook = self.refuse_quarantine_pushes()
        report = self.publish(publish_only=True, attempts=1)
        quarantine = report["raw_publication"]["quarantine"]
        self.assertEqual([row("run-f")], self.remote_rows(), "a quarantine failure never undoes the series")
        self.assertIn("PublishRefused", quarantine["error"])
        self.assertEqual((0, 1), (quarantine["published"], quarantine["pending_records"]))
        self.assertEqual(exact, local.read_bytes())
        self.assertEqual([], self.remote_quarantine())
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual(git(writer, "rev-parse", "refs/remotes/origin/main"), git(writer, "rev-parse", "HEAD"))
        self.assertEqual("", git(writer, "status", "--porcelain"))
        hook.unlink()
        retried = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None, 0), (retried["published"], retried["error"], retried["pending_records"]))
        self.assertEqual([series.portable_quarantine_record(record)], self.remote_quarantine())

    def test_withheld_quarantine_is_published_as_a_stub_and_held_locally(self):
        needle = series._OWNER_PATH_NEEDLE
        raw = needle.encode() + b"alice/\xff"
        record = self.quarantine("run-w", raw)
        local = series.append_quarantine(self.parent, "run-w", [record])["path"]
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (report["published"], report["error"]))
        [stub] = self.remote_quarantine()
        self.assertEqual("owner-path", stub["withheld"]["reason"])
        self.assertEqual((None, None), (stub["inputs"][0]["raw"], stub["inputs"][0]["raw_base64"]))
        self.assertEqual(hashlib.sha256(raw).hexdigest(), stub["inputs"][0]["raw_sha256"])
        writer = self.parent / mirror.WRITER_RELATIVE
        counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                            net_wrapper="/usr/bin/env", attempts=1, run_id="run-w")
        self.assertEqual(1, counted["held_records"])
        self.assertEqual(1, counted["run"]["withheld"])
        held = series.quarantine_dir(self.parent) / series.QUARANTINE_HELD_SUBDIR / local.name
        self.assertEqual([record], series._read_quarantine_file(held), "the only exact copy is kept")
        self.assertEqual([], list((series.quarantine_dir(self.parent) / "published").glob("*.jsonl")))

    def test_a_refusal_naming_a_cell_home_restores_and_its_copy_is_reaped(self):
        # Hermit's cell runner sets HOME inside the cell directory, so a cell
        # result names the needle followed by .local/state/hermit/, and the
        # owner-home pattern publishes the needle plus ".local" as a token.
        prefix = series._OWNER_PATH_NEEDLE + ".local"
        raw = json.dumps({"log_dir": "/srv/cells/cell-1" + prefix + "/state/hermit/verify-failures"}).encode()
        record = self.quarantine("run-l", raw)
        local = series.append_quarantine(self.parent, "run-l", [record])["path"]
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (report["published"], report["error"]))
        [public] = self.remote_quarantine()
        self.assertIsNone(public["withheld"])
        self.assertNotIn(prefix, public["inputs"][0]["raw"])
        self.assertEqual([{"token": "{{OWNER_HOME_1}}", "bytes": len(prefix)}], public["owner_paths"])
        self.assertEqual([(1, raw)], series._quarantined_input_bytes(public))
        writer = self.parent / mirror.WRITER_RELATIVE
        counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                            net_wrapper="/usr/bin/env", attempts=1, run_id="run-l")
        self.assertEqual(0, counted["held_records"])
        self.assertFalse((series.quarantine_held_dir(self.parent) / local.name).exists(),
                         "a record the ledger gives back is not held")
        self.assertEqual([], list((series.quarantine_dir(self.parent) / "published").glob("*.jsonl")))

    def test_a_published_refusal_this_host_cannot_restore_is_held_before_its_copy_is_reaped(self):
        # Published with its content, not as a stub, but its token stands for
        # another account's home, of which the ledger keeps only the length.
        # The candidates are fixed, so the account running the test is never bob.
        self.enterContext(mock.patch.object(
            series, "_owner_home_candidates",
            return_value=[series._OWNER_PATH_NEEDLE + "alice", *series._NON_OWNER_HOME_PREFIXES]))
        plain = self.quarantine("run-u", b"not json")
        foreign = self.quarantine("run-u", (series._OWNER_PATH_NEEDLE + "bob/validate/logs").encode())
        local = series.append_quarantine(self.parent, "run-u", [plain, foreign])["path"]
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((2, None), (report["published"], report["error"]))
        public = {record["event_id"]: record for record in self.remote_quarantine()}
        unrestorable = public[series.portable_quarantine_record(foreign)["event_id"]]
        self.assertIsNone(unrestorable["withheld"])
        self.assertIsNone(series._quarantined_input_bytes(unrestorable),
                          "no prefix this host knows restores the token")
        self.assertIsNotNone(series._quarantined_input_bytes(
            public[series.portable_quarantine_record(plain)["event_id"]]))
        writer = self.parent / mirror.WRITER_RELATIVE
        counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                            net_wrapper="/usr/bin/env", attempts=1, run_id="run-u")
        self.assertEqual(1, counted["held_records"])
        held = series.quarantine_held_dir(self.parent) / local.name
        self.assertEqual([foreign], series._read_quarantine_file(held), "the only exact copy is kept")
        self.assertEqual([], list((series.quarantine_dir(self.parent) / "published").glob("*.jsonl")))

    def test_a_published_refusal_that_does_not_restore_is_retained_again_once_its_held_copy_is_lost(self):
        self.enterContext(mock.patch.object(
            series, "_owner_home_candidates",
            return_value=[series._OWNER_PATH_NEEDLE + "alice", *series._NON_OWNER_HOME_PREFIXES]))
        # One published with a token no candidate restores, one as a stub.
        foreign = self.quarantine("run-r", (series._OWNER_PATH_NEEDLE + "bob/validate/logs").encode())
        withheld = self.quarantine("run-r", series._OWNER_PATH_NEEDLE.encode() + b"alice/\xff")
        records = [foreign, withheld]
        local = series.append_quarantine(self.parent, "run-r", records)["path"]
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((2, None), (report["published"], report["error"]))
        writer = self.parent / mirror.WRITER_RELATIVE
        counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                            net_wrapper="/usr/bin/env", attempts=1, run_id="run-r")
        self.assertEqual((None, 2), (counted["error"], counted["held_records"]))
        held = series.quarantine_held_dir(self.parent) / local.name
        self.assertEqual(records, series._read_quarantine_file(held))

        # While the held copy exists, a replay adds nothing.
        replayed = series.append_quarantine(self.parent, "run-r", records)
        self.assertEqual((0, 2, None), (replayed["written"], replayed["duplicates"],
                                        replayed["published_check_error"]))

        # Once it is gone, the published identity alone does not make the
        # resubmitted exact copies duplicates: the ledger cannot give them back.
        held.unlink()
        resubmitted = series.append_quarantine(self.parent, "run-r", records)
        self.assertEqual((2, 0, None), (resubmitted["written"], resubmitted["duplicates"],
                                        resubmitted["published_check_error"]))
        again = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((0, 2, None), (again["published"], again["already_published"], again["error"]))
        counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                            net_wrapper="/usr/bin/env", attempts=1, run_id="run-r")
        self.assertEqual((None, 2), (counted["error"], counted["held_records"]))
        self.assertEqual(records, series._read_quarantine_file(
            series.quarantine_held_dir(self.parent) / resubmitted["path"].name))
        self.assertEqual(series.union_events([series.portable_quarantine_record(record)
                                              for record in records]),
                         self.remote_quarantine())

    def test_a_local_copy_that_does_not_validate_does_not_make_a_refusal_a_duplicate(self):
        record = self.quarantine("run-m", b"not json")
        # The same event_id, with another input's bytes and digest.
        tampered = copy.deepcopy(record)
        tampered["inputs"] = [series.quarantine_input(1, b"other bytes")]
        self.assertRaises(series.MalformedQuarantine,
                          series.validate_quarantine_records, [tampered], public=False)
        directory = series.quarantine_dir(self.parent)
        series.write_quarantine_file(directory / "published", "run-m.tampered.jsonl",
                                     json.dumps(tampered, sort_keys=True) + "\n")
        retained = series.append_quarantine(self.parent, "run-m", [record])
        self.assertEqual((1, 0), (retained["written"], retained["duplicates"]))
        self.assertEqual([record], series._read_quarantine_file(retained["path"]))

    def test_restoration_tries_only_the_tokens_in_the_inputs_and_each_prefix_once(self):
        # Every distinct owner-home prefix in a record is numbered, its reason
        # included, and restoration used to try every assignment of this host's
        # prefixes to every token: 2**20 hashes for this record. Only the two
        # tokens in its input change its bytes, and no prefix stands for both.
        needle = series._OWNER_PATH_NEEDLE
        mine, cell_home = needle + "carol1", needle + ".local"
        self.assertEqual(len(mine.encode()), len(cell_home.encode()))
        raw = json.dumps({"cwd": mine + "/validate",
                          "log_dir": "/srv/cells/cell-1" + cell_home + "/state"}).encode()
        others = " ".join(f"{needle}u{n:05d}" for n in range(1, 19))
        record = series.quarantine_record(
            stage="projection", run_id="run-t", producer="validate", tree="a" * 40,
            emitted_at="2026-09-21T12:00:00Z", error="MalformedRow",
            reason=f"fixture refusal naming {others}",
            inputs=[series.quarantine_input(1, raw)], host="fixture",
        )
        public = series.portable_quarantine_record(record)
        self.assertIsNone(public["withheld"])
        self.assertEqual({len(mine.encode())}, {entry["bytes"] for entry in public["owner_paths"]})
        self.assertEqual(20, len(public["owner_paths"]))
        hashed = []
        native = hashlib.sha256

        def sha256(data=b""):
            hashed.append(data)
            return native(data)

        with mock.patch.object(series, "_owner_home_candidates", return_value=[cell_home, mine]):
            with mock.patch.object(series.hashlib, "sha256", side_effect=sha256):
                restored = series._quarantined_input_bytes(public)
            self.assertEqual([(1, raw)], restored)
            # The first assignment gives the cell home to the input's first
            # token; the second, the only other, is the received input.
            self.assertEqual(2, len(hashed))
            self.assertTrue(series.published_quarantine_restores(public, record))

    def test_a_record_with_more_input_tokens_than_prefixes_tries_nothing(self):
        needle = series._OWNER_PATH_NEEDLE
        raw = " ".join(f"{needle}u{n:05d}" for n in range(1, 8)).encode()
        public = series.portable_quarantine_record(self.quarantine("run-p", raw))
        self.assertEqual(7, len(public["owner_paths"]))
        with mock.patch.object(series, "_owner_home_candidates",
                               return_value=[needle + "carol1", needle + ".local"]), \
             mock.patch.object(series.hashlib, "sha256", side_effect=AssertionError("hashed")):
            # No prefix stands for two tokens, so two prefixes cannot restore seven.
            self.assertIsNone(series._quarantined_input_bytes(public))

    def test_a_record_whose_prefixes_could_take_too_many_tries_is_not_tried(self):
        needle = series._OWNER_PATH_NEEDLE
        prefixes = [f"{needle}carol{n}" for n in range(1, 10)]
        record = self.quarantine("run-c", f"{prefixes[0]}/a {prefixes[1]}/b".encode())
        public = series.portable_quarantine_record(record)
        self.assertEqual(2, len(public["owner_paths"]))
        with mock.patch.object(series, "_owner_home_candidates", return_value=prefixes), \
             mock.patch.object(series.hashlib, "sha256", side_effect=AssertionError("hashed")):
            # Nine prefixes fit each of the two tokens: 81 assignments, more than 64.
            with self.assertRaisesRegex(series.QuarantineRestoreLimit, "could take 81 prefix assignments"):
                series._quarantined_input_bytes(public)
            self.assertFalse(series.published_quarantine_restores(public, record))

    def test_a_published_refusal_whose_restoration_is_not_tried_is_held_before_its_copy_is_reaped(self):
        needle = series._OWNER_PATH_NEEDLE
        prefixes = [f"{needle}carol{n}" for n in range(1, 4)]
        record = self.quarantine("run-n", f"{prefixes[0]}/validate/logs".encode())
        local = series.append_quarantine(self.parent, "run-n", [record])["path"]
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (report["published"], report["error"]))
        writer = self.parent / mirror.WRITER_RELATIVE
        # Three prefixes fit the token, so with at most two tries allowed
        # restoration is not tried and the ledger is not trusted to give it back.
        with mock.patch.object(series, "_owner_home_candidates", return_value=prefixes), \
             mock.patch.object(series, "QUARANTINE_RESTORE_ATTEMPTS", 2):
            counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                                net_wrapper="/usr/bin/env", attempts=1, run_id="run-n")
        self.assertEqual((None, 1), (counted["error"], counted["held_records"]))
        held = series.quarantine_held_dir(self.parent) / local.name
        self.assertEqual([record], series._read_quarantine_file(held), "the only exact copy is kept")
        self.assertEqual([], list((series.quarantine_dir(self.parent) / "published").glob("*.jsonl")))

    def test_a_held_copy_is_synced_before_its_retained_copy_goes_even_when_already_held(self):
        # A first reap renamed the held copy into place and then failed to sync
        # held/. The retry finds the record already held, and must still make
        # that copy durable before it deletes the only other one.
        record = self.quarantine("run-y", series._OWNER_PATH_NEEDLE.encode() + b"alice/\xff")
        local = series.append_quarantine(self.parent, "run-y", [record])["path"]
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (report["published"], report["error"]))
        retained = series.quarantine_dir(self.parent) / "published" / local.name
        held = series.quarantine_held_dir(self.parent)
        self.assertTrue(retained.exists())
        writer = self.parent / mirror.WRITER_RELATIVE
        native = series._fsync_quarantine_directory

        def failing(directory):
            if Path(directory) == held:
                raise OSError(errno.EIO, "Input/output error")
            native(directory)

        with mock.patch.object(series, "_fsync_quarantine_directory", side_effect=failing):
            failed = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                               net_wrapper="/usr/bin/env", attempts=1, run_id="run-y")
        self.assertIn("Input/output error", failed["error"])
        self.assertEqual([record], series._read_quarantine_file(held / local.name),
                         "renamed into place before the sync failed")
        self.assertTrue(retained.exists())
        synced = []

        def recording(directory):
            synced.append((Path(directory), retained.exists()))
            native(directory)

        with mock.patch.object(series, "_fsync_quarantine_directory", side_effect=recording):
            counted = mirror.publish_quarantine(self.parent, self.ledger, writer,
                                                net_wrapper="/usr/bin/env", attempts=1, run_id="run-y")
        self.assertEqual((None, 1), (counted["error"], counted["held_records"]))
        self.assertFalse(retained.exists())
        self.assertIn((held, True), synced, "held/ is synced while the retained copy still exists")
        self.assertIn((held.parent, True), synced, "and so is held/'s own entry")
        self.assertEqual([record], series._read_quarantine_file(held / local.name))

    def test_a_new_quarantine_directory_is_synced_into_its_parent(self):
        directory = series.quarantine_dir(self.parent)
        self.assertFalse(directory.exists())
        native = series._fsync_quarantine_directory
        synced = []

        def recording(path):
            synced.append(Path(path))
            native(path)

        with mock.patch.object(series, "_fsync_quarantine_directory", side_effect=recording):
            kept = series.append_quarantine(self.parent, "run-s", [self.quarantine("run-s", b"not json")])
        self.assertEqual(1, kept["written"])
        self.assertEqual(directory, synced[-1], "the refusal's own entry")
        self.assertIn(directory.parent, synced[:-1], "the new directory's entry, before the refusal's")

    def test_append_never_waits_on_a_publisher_holding_the_quarantine_lock(self):
        # The publisher holds the exclusive quarantine lock across its network
        # push. An append used to wait for it (at most 120 s) and lose the
        # refused input it held in memory on timeout.
        directory = series.quarantine_dir(self.parent)
        directory.mkdir(parents=True, exist_ok=True)
        record = self.quarantine("run-busy", b"not json")
        with series.series_spool_transition_lock(directory, exclusive=True, timeout_seconds=5), \
             mock.patch.object(series, "QUARANTINE_LOCK_TIMEOUT_SECONDS", 0.5):
            started = time.monotonic()
            retained = series.append_quarantine(self.parent, "run-busy", [record])
            elapsed = time.monotonic() - started
        self.assertEqual(1, retained["written"])
        self.assertLess(elapsed, 5.0)
        self.assertEqual([record], series._read_quarantine_file(retained["path"]))
        self.assertEqual([], list(directory.glob(".*.partial")), "no partial file is left behind")
        # Two appends never share a file, so neither can tear the other.
        second = series.append_quarantine(
            self.parent, "run-busy", [self.quarantine("run-busy", b"other")])["path"]
        self.assertNotEqual(retained["path"], second)

    def test_an_append_during_publication_survives_the_drain_and_publishes_next(self):
        first = self.quarantine("run-a", b"first")
        series.append_quarantine(self.parent, "run-a", [first])
        late = self.quarantine("run-late", b"late")
        original = mirror.publisher._store_snapshot
        appended: list[Path] = []

        def snapshot_then_append(store, spool, *args, **kwargs):
            result = original(store, spool, *args, **kwargs)
            if store == "series-quarantine" and not appended:
                appended.append(series.append_quarantine(self.parent, "run-late", [late])["path"])
            return result

        with mock.patch.object(mirror.publisher, "_store_snapshot", side_effect=snapshot_then_append):
            report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None, 1), (report["published"], report["error"], report["pending_records"]))
        self.assertEqual([series.portable_quarantine_record(first)], self.remote_quarantine())
        [late_path] = appended
        self.assertTrue(late_path.exists(), "a file written after the snapshot is not drained")
        again = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, 0), (again["published"], again["pending_records"]))
        self.assertEqual(series.union_events([series.portable_quarantine_record(first),
                                              series.portable_quarantine_record(late)]),
                         self.remote_quarantine())

    def test_a_truncated_quarantine_file_is_set_aside_and_the_others_publish(self):
        directory = series.quarantine_dir(self.parent)
        directory.mkdir(parents=True, exist_ok=True)
        whole = json.dumps(self.quarantine("run-trunc", b"x"), sort_keys=True)
        truncated = directory / "run-trunc.1-1-00000000.jsonl"
        truncated.write_text(whole[: len(whole) // 2])
        exact = truncated.read_bytes()
        (directory / "published").mkdir()
        stale = directory / "published" / "run-old.1-1-00000000.jsonl"
        stale.write_bytes(b"\xff\xfe not utf-8\n")
        # An append beside the unreadable files still works ...
        good = self.quarantine("run-good", b"not json")
        self.assertEqual(1, series.append_quarantine(self.parent, "run-good", [good])["written"])
        # ... and publication moves them aside instead of refusing the store.
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None, 0), (report["published"], report["error"], report["pending_records"]))
        self.assertEqual(["pending--run-trunc.1-1-00000000.jsonl",
                          "published--run-old.1-1-00000000.jsonl"], report["set_aside"])
        self.assertEqual(2, report["corrupt_files"])
        self.assertEqual([series.portable_quarantine_record(good)], self.remote_quarantine())
        corrupt = directory / series.QUARANTINE_CORRUPT_SUBDIR
        self.assertEqual(exact, (corrupt / "pending--run-trunc.1-1-00000000.jsonl").read_bytes(),
                         "set aside byte for byte, never rewritten")
        self.assertFalse(truncated.exists() or stale.exists())
        # Nothing republishes or deletes corrupt/.
        later = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual(([], 2), (later["set_aside"], later["corrupt_files"]))

    def test_recovery_republishes_an_unpushed_quarantine_append_and_refuses_others(self):
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        spooled = self.quarantine("run-r", b"not json")
        series.append_quarantine(self.parent, "run-r", [spooled])
        shard = writer / self.QUARANTINE_SHARD

        def commit(records):
            shard.parent.mkdir(parents=True, exist_ok=True)
            shard.write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in records))
            git(writer, "add", "quarantine")
            git(writer, "commit", "-qm", "series-quarantine: append event(s)")
            return git(writer, "rev-parse", "HEAD")

        # Violating: the unpushed commit adds a record that is not spooled here.
        foreign = self.quarantine("run-other", b"never spooled")
        refused = commit([series.portable_quarantine_record(foreign)])
        with self.assertRaisesRegex(mirror.MirrorRefused, "unchanged spooled quarantine append") as caught:
            self.publish(publish_only=True)
        self.assertEqual(refused, git(writer, "rev-parse", "HEAD"))
        self.assertEqual([], self.remote_quarantine())
        # Publishing would move the writer off work it cannot account for, so
        # the quarantine is not attempted; it is still counted.
        blocked = caught.exception.quarantine
        self.assertTrue(blocked["error"].startswith("not attempted: "), blocked)
        self.assertEqual((0, 1), (blocked["published"], blocked["pending_records"]))
        git(writer, "reset", "-q", "--hard", "refs/remotes/origin/main")

        # Qualifying: a crash between commit and push left exactly the spooled records.
        crashed = commit([series.portable_quarantine_record(spooled)])
        report = self.publish(publish_only=True)
        self.assertEqual(f"refs/heads/recovery/cell-series-{crashed}",
                         report["raw_publication"]["recovered_ref"])
        self.assertIsNone(report["raw_publication"]["quarantine"]["error"])
        self.assertEqual([series.portable_quarantine_record(spooled)], self.remote_quarantine())

    def test_recovery_republishes_an_unpushed_month_shard_append_as_hourly_shards(self):
        # Code before hourly shards committed appends to a month shard; a writer
        # left with such an unpushed commit must recover onto the hourly layout
        # and never push the month shard it appended to.
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        spooled = self.quarantine("run-m", b"not json")
        series.append_quarantine(self.parent, "run-m", [spooled])
        month = "quarantine/series/hermit/fixture/2026-09.jsonl"
        (writer / month).parent.mkdir(parents=True, exist_ok=True)
        (writer / month).write_text(json.dumps(series.portable_quarantine_record(spooled), sort_keys=True) + "\n")
        git(writer, "add", "quarantine")
        git(writer, "commit", "-qm", "series-quarantine: append 1 event(s)")
        crashed = git(writer, "rev-parse", "HEAD")

        report = self.publish(publish_only=True)
        self.assertEqual(f"refs/heads/recovery/cell-series-{crashed}",
                         report["raw_publication"]["recovered_ref"])
        self.assertIsNone(report["raw_publication"]["quarantine"]["error"])
        self.assertEqual([series.portable_quarantine_record(spooled)], self.remote_quarantine())
        self.assertEqual([self.QUARANTINE_SHARD],
                         git(self.remote, "ls-tree", "-r", "--name-only", "main", "quarantine/series").splitlines())

    def test_recovery_accepts_an_unpushed_v3_append_only_with_exactly_its_input_files(self):
        # A refusal already published, in another hour's shard, with its file.
        first = self.quarantine("run-first", b"first", emitted_at="2026-09-21T11:00:00Z")
        series.append_quarantine(self.parent, "run-first", [first])
        self.publish(publish_only=True)
        [published_file] = self.remote_input_files()
        writer = self.parent / mirror.WRITER_RELATIVE
        spooled = self.quarantine("run-v3", b"not json")
        series.append_quarantine(self.parent, "run-v3", [spooled])
        slim, files = series.slim_quarantine_record(series.portable_quarantine_record(spooled))
        [(ref, content)] = files.items()
        stray = series.quarantine_input_path(hashlib.sha256(b"no row names me").hexdigest())

        def commit(stored: dict[str, bytes | None], row: dict = slim):
            shard = writer / self.QUARANTINE_SHARD
            shard.parent.mkdir(parents=True, exist_ok=True)
            shard.write_text(json.dumps(row, sort_keys=True) + "\n")
            for path, data in stored.items():
                if data is None:
                    (writer / path).unlink()
                    continue
                (writer / path).parent.mkdir(parents=True, exist_ok=True)
                (writer / path).write_bytes(data)
            git(writer, "add", "--all", "quarantine")
            git(writer, "commit", "-qm", "series-quarantine: append 1 event(s)")
            return git(writer, "rev-parse", "HEAD")

        # Violating: each commit adds the spooled v3 row, but not with exactly
        # the files it names, or it touches a file already published, or it
        # adds a v3 row under the spooled identity that is not its slim form.
        for label, stored, row, refusal in (
            ("its file missing", {}, slim, "input files its spooled quarantine rows name"),
            ("its file with other bytes", {ref: content + b"!"}, slim,
             "input files its spooled quarantine rows name"),
            ("a file no row names", {ref: content, stray: b"no row names me"}, slim,
             "input files its spooled quarantine rows name"),
            ("a published file changed", {ref: content, published_file: b"other"}, slim,
             "changes quarantine input"),
            ("a published file removed", {ref: content, published_file: None}, slim,
             "changes quarantine input"),
            ("its row altered", {ref: content}, {**slim, "error": "Altered"},
             "not an unchanged spooled quarantine append"),
        ):
            with self.subTest(label):
                refused = commit(stored, row)
                with self.assertRaisesRegex(mirror.MirrorRefused, refusal):
                    self.publish(publish_only=True)
                self.assertEqual(refused, git(writer, "rev-parse", "HEAD"))
                self.assertEqual([], self.remote_quarantine())
                git(writer, "reset", "-q", "--hard", "refs/remotes/origin/main")

        # Qualifying: a crash between commit and push left the spooled v3 row
        # and its file.
        crashed = commit({ref: content})
        report = self.publish(publish_only=True)
        self.assertEqual(f"refs/heads/recovery/cell-series-{crashed}",
                         report["raw_publication"]["recovered_ref"])
        self.assertIsNone(report["raw_publication"]["quarantine"]["error"])
        self.assertEqual([slim], self.remote_quarantine_rows())
        self.assertEqual([series.portable_quarantine_record(spooled)], self.remote_quarantine())
        self.assertEqual(sorted([published_file, ref]), self.remote_input_files())

    def test_an_input_file_already_published_is_shared_not_written_again(self):
        first = self.quarantine("run-one", b"same bytes")
        series.append_quarantine(self.parent, "run-one", [first])
        self.publish(publish_only=True)
        [shared] = self.remote_input_files()
        blob = git(self.remote, "rev-parse", f"main:{shared}")
        second = self.quarantine("run-two", b"same bytes")
        self.assertNotEqual(first["event_id"], second["event_id"])
        series.append_quarantine(self.parent, "run-two", [second])
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (report["published"], report["error"]))
        appended = git(self.remote, "log", "-1", "--format=%H", "main", "--", self.QUARANTINE_SHARD)
        self.assertEqual([self.QUARANTINE_SHARD],
                         git(self.remote, "diff-tree", "--no-commit-id", "--name-only", "-r",
                             appended).splitlines())
        self.assertEqual(([shared], blob), (self.remote_input_files(),
                                            git(self.remote, "rev-parse", f"main:{shared}")))
        by_identity = lambda records: sorted(records, key=lambda record: record["event_id"])
        self.assertEqual(by_identity([series.portable_quarantine_record(first),
                                      series.portable_quarantine_record(second)]),
                         by_identity(self.remote_quarantine()))

    def test_a_published_input_file_with_other_bytes_refuses_and_keeps_the_spool(self):
        record = self.quarantine("run-c", b"the received bytes")
        path = series.quarantine_input_path(hashlib.sha256(b"the received bytes").hexdigest())
        other = self.root / "conflicting-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        (other / path).parent.mkdir(parents=True, exist_ok=True)
        (other / path).write_bytes(b"other bytes under that name")
        git(other, "add", "quarantine")
        git(other, "commit", "-qm", "a file that does not hash to its name")
        git(other, "push", "-q", "origin", "main")
        local = series.append_quarantine(self.parent, "run-c", [record])["path"]
        exact = local.read_bytes()
        report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertIn(f"{path}: the published file does not hold the bytes", report["error"])
        self.assertEqual((0, 1), (report["published"], report["pending_records"]))
        self.assertEqual(exact, local.read_bytes())
        self.assertEqual([], self.remote_quarantine_rows())
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("", git(writer, "status", "--porcelain"))

    def test_a_record_with_an_input_over_the_file_ceiling_publishes_whole(self):
        record = self.quarantine("run-whole", b"not json")
        series.append_quarantine(self.parent, "run-whole", [record])
        with mock.patch.object(mirror.publisher._series_store(),
                               "QUARANTINE_INPUT_FILE_CEILING_BYTES", len(b"not json") - 1):
            report = self.publish(publish_only=True)["raw_publication"]["quarantine"]
        self.assertEqual((1, None), (report["published"], report["error"]))
        self.assertEqual([series.portable_quarantine_record(record)], self.remote_quarantine_rows())
        self.assertEqual([], self.remote_input_files())

    def test_a_retained_refusal_is_kept_until_its_published_input_files_are_read(self):
        record = self.quarantine("run-k", b"not json")
        local = series.append_quarantine(self.parent, "run-k", [record])["path"]
        self.publish(publish_only=True)
        retained = local.parent / "published" / local.name
        self.assertTrue(retained.exists())
        writer = self.parent / mirror.WRITER_RELATIVE
        # The source root reads the row but not its file: the copy stays.
        with mock.patch.object(mirror.publisher, "_published_input_files",
                               side_effect=lambda repo, commit, paths, files=None: dict.fromkeys(paths)):
            mirror.publish_quarantine(self.parent, self.ledger, writer,
                                      net_wrapper="/usr/bin/env", attempts=1)
        self.assertTrue(retained.exists(), "kept while its input file is not readable")
        mirror.publish_quarantine(self.parent, self.ledger, writer,
                                  net_wrapper="/usr/bin/env", attempts=1)
        self.assertFalse(retained.exists(), "reaped once the row expands")

    def test_scheduled_backlog_publishes_a_quarantine_only_backlog(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )
        native_run = subprocess.run

        def sweep_child(command, **kwargs):
            if str(MODULE) in command:
                command = bounded_fixture_command(
                    [sys.executable, "-B", "-c", bootstrap, *command[1:]])
            return native_run(command, **kwargs)

        # Every row of the run was refused: no series batch triggers a publish.
        record = self.quarantine("run-only-refused", b"not json")
        series.append_quarantine(self.parent, "run-only-refused", [record])
        self.assertEqual({"series-quarantine": series.quarantine_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))
        hook = self.refuse_quarantine_pushes()
        with mock.patch.object(publish_backlog, "STATE_ROOT", self.parent), \
             mock.patch.object(publish_backlog.subprocess, "run", side_effect=sweep_child):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                failed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
            failure = json.loads(output.getvalue())
            # The mirror exits 0 (its series had nothing to publish); the store
            # is judged by its own quarantine report.
            self.assertEqual(1, failed)
            self.assertEqual(1, failure["failed_stores"])
            [store] = failure["reports"]
            self.assertEqual(("series-quarantine", 1), (store["store"], store["rc"]))
            self.assertIn("PublishRefused", store["error"])
            self.assertEqual([], self.remote_quarantine())
            hook.unlink()
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                passed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
            delivered = json.loads(output.getvalue())
        self.assertEqual(0, passed)
        self.assertEqual((0, 1), (delivered["failed_stores"], delivered["published"]))
        self.assertEqual([series.portable_quarantine_record(record)], self.remote_quarantine())
        self.assertEqual({}, publish_backlog.pending_stores(self.parent))

    def test_quarantine_outcome_never_reads_a_missing_report_as_published(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        self.assertEqual({"published": 2}, publish_backlog.quarantine_outcome(
            {"raw_publication": {"quarantine": {"published": 2, "error": None}}}))
        self.assertEqual(1, publish_backlog.quarantine_outcome({"raw_publication": {}})["rc"])
        self.assertEqual(1, publish_backlog.quarantine_outcome(
            {"raw_publication": {"quarantine": {"published": 0, "error": "push refused"}}})["rc"])

    def test_scheduled_backlog_retries_failed_push_without_another_validation(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        # Execute the real sweep, mirror CLI and Git transport. Only the row
        # checker is a fixture double, as in the normal-completion test above.
        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )
        native_run = subprocess.run

        def sweep_child(command, **kwargs):
            if str(MODULE) in command:
                command = bounded_fixture_command(
                    [sys.executable, "-B", "-c", bootstrap, *command[1:]])
            return native_run(command, **kwargs)

        batch = self.append("completed-before-push-failure", "diverged")
        expected = batch.read_bytes()
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\necho deliberate-push-refusal >&2\nexit 1\n")
        hook.chmod(0o755)
        with mock.patch.object(publish_backlog, "STATE_ROOT", self.parent), \
             mock.patch.object(publish_backlog.subprocess, "run", side_effect=sweep_child):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                failed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
            failure = json.loads(output.getvalue())
            self.assertEqual(1, failed)
            self.assertEqual(1, failure["failed_stores"])
            self.assertNotEqual(0, failure["reports"][0]["rc"])
            self.assertEqual(expected, batch.read_bytes())
            self.assertEqual([], self.remote_rows())
            writer = self.parent / mirror.WRITER_RELATIVE
            refused_commit = git(writer, "rev-parse", "HEAD")
            hook.unlink()
            # No new append, validation or measured result is produced here.
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                passed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
            delivered = json.loads(output.getvalue())
        self.assertEqual(0, passed)
        self.assertEqual(0, delivered["failed_stores"])
        self.assertEqual(1, delivered["published"])
        self.assertEqual([row("completed-before-push-failure", "diverged")], self.remote_rows())
        git(self.remote, "merge-base", "--is-ancestor", delivered["reports"][0]["commit"], "main")
        self.assertFalse(batch.exists())
        recovery = delivered["reports"][0]["raw_publication"]["recovered_ref"]
        self.assertEqual(f"refs/heads/recovery/cell-series-{refused_commit}", recovery)
        self.assertEqual(refused_commit, git(writer, "rev-parse", recovery))

    # --- Scorecard renderer, head order and proof independence ------------

    def set_tool(self, body: str, checkout: Path | None = None) -> str:
        """Replace a fixture scorecard renderer and commit it; return its HEAD."""
        checkout = checkout or self.tool
        tool = checkout / "ci/compat-envelope/scorecard.rs"
        tool.parent.mkdir(parents=True, exist_ok=True)
        tool.write_text("#!/usr/bin/python3\nimport os, pathlib, subprocess, sys\n" + HELP_FIXTURE
                        + "fd=int(os.environ['DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD'])\nos.fstat(fd)\n"
                        + "ledger=pathlib.Path(os.environ['DEV_HERMIT_TEST_LEDGER_ROOT'])\n"
                        + "p=ledger/'scorecard'\n" + body)
        tool.chmod(0o755)
        git(checkout, "add", "ci")
        git(checkout, "commit", "-qm", "fixture renderer")
        return git(checkout, "rev-parse", "HEAD")

    def use_head_renderer(self) -> None:
        # The renderer runs in the measured checkout; it names the head it rendered.
        self.set_tool("head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()\n"
                      "(p/'SCORECARD.md').write_text('rendered at '+head+'\\n')\n")

    def source_commit(self, message: str, when: str, *, base: str | None = None) -> str:
        """Commit in the measured repository at WHEN, optionally on top of BASE."""
        if base is not None:
            git(self.source, "checkout", "-q", "--detach", base)
        environment = {**os.environ, "GIT_COMMITTER_DATE": when, "GIT_AUTHOR_DATE": when}
        subprocess.run(["git", "-C", str(self.source), "commit", "--allow-empty", "-qm", message],
                       check=True, env=environment)
        return git(self.source, "rev-parse", "HEAD")

    def publish_at(self, commit: str, **kwargs):
        git(self.source, "checkout", "-q", "--detach", commit)
        self.target = commit
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish(**kwargs)
        return report, stderr.getvalue()

    def remote_file(self, path: str) -> str | None:
        found = subprocess.run(["git", "-C", str(self.remote), "show", f"main:{path}"],
                               capture_output=True, text=True, check=False)
        return found.stdout if found.returncode == 0 else None

    def remote_latest(self) -> dict | None:
        text = self.remote_file("latest.json")
        return None if text is None else json.loads(text)

    def test_publish_only_reports_the_series_source_without_reading_its_rows(self):
        # Reading every row only to count them was 191.6 s of a 218.7 s warm
        # publish-only run on the 2026-10-12 ledger.
        self.append("publish-only-source")
        with mock.patch.object(mirror, "_published_snapshot",
                               side_effect=AssertionError("a publish-only run read every row")):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertNotIn("rows", report)
        self.assertEqual(git(self.remote, "rev-parse", "main"), report["series_commit"])
        self.assertEqual(report["series_commit"], report["commit"])
        self.assertEqual(git(self.remote, "rev-parse", "main:series"), report["source_tree"])
        self.assertEqual([row("publish-only-source")], self.remote_rows())

    def test_parser_leaves_renderer_to_the_measured_checkout(self):
        args = mirror.build_parser().parse_args(
            ["--parent", "/p", "--source-checkout", "/s", "--target", "0" * 40])
        self.assertIsNone(args.tool_checkout)
        explicit = mirror.build_parser().parse_args(
            ["--parent", "/p", "--source-checkout", "/s", "--target", "0" * 40, "--tool-checkout", "/t"])
        self.assertEqual(Path("/t"), explicit.tool_checkout)

    def test_measured_checkout_renders_its_own_scorecard_by_default(self):
        # https://github.com/rrnewton/dev-hermit/issues/392: the measured
        # SCORECARD.md and scorecard.rs both differ from the pinned tool.
        self.set_tool("if pathlib.Path('SCORECARD.md').read_text() != 'pinned prose\\n':\n"
                      "    sys.exit('SCORECARD.md is stale for this renderer')\n"
                      "(p/'SCORECARD.md').write_text('rendered by pinned renderer\\n')\n")
        (self.source / "SCORECARD.md").write_text("measured prose\n")
        (self.source / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "nightly-2026-07-12"\n')
        git(self.source, "add", "SCORECARD.md", "rust-toolchain.toml")
        git(self.source, "commit", "-qm", "measured prose")
        measured = self.set_tool("assert pathlib.Path('SCORECARD.md').read_text() == 'measured prose\\n'\n"
                                 "(p/'SCORECARD.md').write_text('rendered by measured renderer\\n')\n",
                                 checkout=self.source)
        cargo = self.root / "bin/cargo"
        cargo.write_text(
            "#!/usr/bin/python3\nimport os, sys\n"
            "manifest = sys.argv[sys.argv.index('--manifest-path') + 1]\n"
            f"assert manifest in {[str(self.tool / 'Cargo.toml'), str(self.source / 'Cargo.toml')]!r}, manifest\n"
            "open(os.environ['CARGO_FIXTURE_LOG'], 'a').write(manifest + '\\n')\n"
            "assert os.environ['RUSTUP_TOOLCHAIN'] == 'nightly-2026-07-12-x86_64-unknown-linux-gnu'\n")
        log = self.root / "cargo.log"
        self.enterContext(mock.patch.dict(os.environ, {"CARGO_FIXTURE_LOG": str(log)}))
        self.append("pinned-renderer")
        pinned, _ = self.publish_at(measured)
        self.assertEqual("derived-failed", pinned["state"])
        self.assertIn("SCORECARD.md is stale", pinned["scorecard"]["error"])
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.append("measured-renderer")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = mirror.mirror(self.parent, self.source, measured, mirror.REMOTE_URL,
                                   net_wrapper="/usr/bin/env")
        self.assertEqual("published", report["state"], (report, stderr.getvalue()))
        self.assertEqual(measured, report["scorecard"]["tool_hermit_commit"])
        self.assertEqual("rendered by measured renderer\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertEqual(measured, self.remote_latest()["scorecard_tool_hermit_commit"])
        self.assertEqual(str(self.source / "Cargo.toml"), log.read_text().splitlines()[-1])

    def test_renderer_writes_outside_its_output_are_refused_before_push(self):
        # The renderer is measured code: it may write exactly SCORECARD.md and cells.json.
        self.set_tool("(p/'SCORECARD.md').write_text('rendered\\n')\n"
                      "(p/'finalized/runs').mkdir(parents=True)\n"
                      "(p/'finalized/runs/forged.json').write_text('{}\\n')\n")
        self.append("forged-path")
        report, _ = self.publish_at(self.target)
        self.assertEqual("derived-failed", report["state"])
        self.assertEqual("failed", report["scorecard"]["state"])
        self.assertIn("changed ledger paths outside its generated output", report["scorecard"]["error"])
        self.assertIn("scorecard/finalized/runs/forged.json", report["scorecard"]["error"])
        self.assertEqual("", git(self.remote, "ls-tree", "-r", "main", "--", "scorecard/finalized"))
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertIsNone(self.remote_latest())
        self.assertEqual([row("forged-path")], self.remote_rows())

    def test_renderer_commit_in_its_ledger_copy_is_refused_before_push(self):
        # The ledger the renderer is given is a throwaway copy: a commit there,
        # or a push from it, reaches neither the writer nor the remote.
        self.set_tool("(p/'SCORECARD.md').write_text('rendered\\n')\n"
                      "subprocess.run(['git','-C',str(ledger),'commit','--allow-empty','-qm','renderer forged commit'],"
                      "check=True)\n"
                      "pushed=subprocess.run(['git','-C',str(ledger),'push','origin','HEAD:refs/heads/forged'],"
                      "capture_output=True)\n"
                      "assert pushed.returncode != 0, pushed\n")
        self.append("forged-commit")
        report, _ = self.publish_at(self.target)
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("moved the HEAD of its ledger copy; nothing was copied", report["scorecard"]["error"])
        self.assertNotIn("renderer forged commit", git(self.remote, "log", "--format=%s", "--all"))
        self.assertIsNone(self.remote_latest())
        self.assertEqual([row("forged-commit")], self.remote_rows())
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual(report["series_commit"], git(writer, "rev-parse", "HEAD"))
        self.assertEqual("", git(writer, "status", "--porcelain"))
        self.assertEqual([], list(writer.parent.glob(mirror.RENDER_COPY_PREFIX + "*")))

    # --- Scorecard history stored in parts (scorecard_cells) ---------------

    def test_parts_layout_renders_through_a_view_that_never_leaves_the_copy(self):
        # The first publication stores the history in parts. The renderer of the
        # second sees the single file at the same commit name, through a
        # replacement that exists only in its throwaway copy.
        self.append("first-parts")
        self.assertEqual("published", self.publish()["state"])
        seen = self.root / "renderer-saw.json"
        rendered = b'{"rendered":"from parts"}\n'
        self.set_tool(
            "import json\n"
            "def run(*args, **kwargs):\n"
            " return subprocess.run(['git','-C',str(ledger),*args],capture_output=True,**kwargs)\n"
            "stored=(p/'cells.json').read_bytes()\n"
            "real=run('ls-tree','-r','--name-only','HEAD','scorecard',"
            "env={**os.environ,'GIT_NO_REPLACE_OBJECTS':'1'})\n"
            "pushed=run('push','origin','refs/replace/*:refs/replace/*')\n"
            f"pathlib.Path({str(seen)!r}).write_text(json.dumps({{"
            "'stored':stored.decode(),'shown':run('show','HEAD:scorecard/cells.json').stdout.decode(),"
            "'head':run('rev-parse','HEAD').stdout.decode().strip(),"
            "'replacements':run('for-each-ref','--format=%(refname) %(objectname)','refs/replace')"
            ".stdout.decode().splitlines(),"
            "'real':real.stdout.decode().splitlines(),'pushed':pushed.returncode,"
            "'parts_on_disk':(p/'cells').exists()}))\n"
            f"(p/'cells.json').write_bytes({rendered!r})\n"
            "(p/'SCORECARD.md').write_text('rendered from parts\\n')\n")
        self.append("second-parts")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        saw = json.loads(seen.read_text())
        base = report["series_commit"]
        self.assertEqual(base, report["source_commit"])
        self.assertEqual('{"legacy":"preserve"}\n', saw["stored"])
        self.assertEqual(saw["stored"], saw["shown"])
        self.assertEqual(base, saw["head"])
        self.assertEqual(1, len(saw["replacements"]), saw)
        replaced, view = saw["replacements"][0].split()
        self.assertEqual(f"refs/replace/{base}", replaced)
        self.assertEqual(["scorecard/SCORECARD.md", "scorecard/cells/manifest.json"], saw["real"])
        self.assertNotEqual(0, saw["pushed"])
        self.assertFalse(saw["parts_on_disk"])
        self.assertEqual(rendered, stored_history(self.remote))
        self.assertEqual("", git(self.remote, "ls-tree", "--name-only", "main", "--", "scorecard/cells.json"))
        self.assertEqual("rendered from parts", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        # latest.json keeps naming the history by its logical path, with the
        # digest of the whole reassembled document.
        self.assertEqual(hashlib.sha256(rendered).hexdigest(),
                         json.loads(git(self.remote, "show", "main:latest.json"))["scorecard"]["scorecard/cells.json"])
        writer = self.parent / mirror.WRITER_RELATIVE
        for repository in (self.remote, self.ledger, writer):
            with self.subTest(repository=repository.name):
                self.assertEqual("", git(repository, "for-each-ref", "refs/replace"))
                missing = subprocess.run(["git", "-C", str(repository), "cat-file", "-e", view],
                                         capture_output=True, check=False)
                self.assertNotEqual(0, missing.returncode)
        self.assertEqual("", git(writer, "status", "--porcelain"))
        self.assertEqual([], list(writer.parent.glob(mirror.RENDER_COPY_PREFIX + "*")))

    def test_history_parts_follow_the_rendered_size(self):
        # A history one byte over the part ceiling needs two parts. When it
        # shrinks, the part it no longer needs is deleted in the same commit.
        ceiling = mirror.scorecard_cells.FILE_CEILING_BYTES
        size_file = self.root / "history-size"
        self.set_tool(f"size=int(pathlib.Path({str(size_file)!r}).read_text())\n"
                      "(p/'cells.json').write_bytes(b'{' + b' ' * (size - 2) + b'}')\n"
                      "(p/'SCORECARD.md').write_text('sized %d\\n' % size)\n")
        for step, (size, parts) in enumerate(((ceiling + 1, [ceiling, 1]), (7, [7]))):
            with self.subTest(size=size):
                size_file.write_text(str(size))
                self.append(f"sized-{step}")
                report = self.publish()
                self.assertEqual("published", report["state"], report)
                self.assertEqual(b"{" + b" " * (size - 2) + b"}", stored_history(self.remote))
                manifest = json.loads(git(self.remote, "show", "main:scorecard/cells/manifest.json"))
                self.assertEqual(parts, [part["bytes"] for part in manifest["parts"]])
                self.assertEqual(["scorecard/cells/manifest.json"],
                                 git(self.remote, "ls-tree", "-r", "--name-only", "main", "--",
                                     "scorecard/cells.json", "scorecard/cells").splitlines())
                # The parts are in the parts commit the manifest names, and a
                # part the smaller history no longer needs is not there.
                self.assertEqual([mirror.scorecard_cells.part_path(index) for index in range(len(parts))],
                                 git(self.remote, "ls-tree", "-r", "--name-only",
                                     manifest["parts_commit"]).splitlines())

    def test_history_parts_live_on_the_history_ref_and_main_keeps_only_the_manifest(self):
        # Storage audit 2026-10-12: every regeneration's parts on main were
        # 8.49 GiB of the 9.69 GiB packed ledger. They now go to a parentless
        # commit on refs/heads/scorecard-history, replaced each time, pushed
        # atomically with the main commit whose manifest names it.
        cells = mirror.scorecard_cells
        documents = [b'{"generation":1}\n', b'{"generation":2}\n']
        source = self.root / "history-document"
        self.set_tool(f"(p/'cells.json').write_bytes(pathlib.Path({str(source)!r}).read_bytes())\n"
                      "(p/'SCORECARD.md').write_text('regenerated\\n')\n")
        seen = []
        for step, document in enumerate(documents):
            with self.subTest(step=step):
                source.write_bytes(document)
                self.append(f"external-{step}")
                report = self.publish()
                self.assertEqual("published", report["state"], report)
                manifest = json.loads(git(self.remote, "show", f"main:{cells.MANIFEST_PATH}"))
                self.assertEqual(cells.EXTERNAL_MANIFEST_SCHEMA, manifest["schema"])
                parts = manifest["parts_commit"]
                self.assertEqual([cells.MANIFEST_PATH], git(
                    self.remote, "ls-tree", "-r", "--name-only", "main", "--",
                    cells.LOGICAL_PATH, cells.PARTS_DIRECTORY).splitlines())
                self.assertEqual(parts, git(self.remote, "rev-parse", cells.HISTORY_REF))
                self.assertEqual(parts, git(self.remote, "rev-list", "--parents", "-n", "1", parts))
                self.assertEqual([cells.part_path(0)], git(
                    self.remote, "ls-tree", "-r", "--name-only", parts).splitlines())
                self.assertEqual(document, stored_history(self.remote))
                self.assertEqual(hashlib.sha256(document).hexdigest(),
                                 json.loads(git(self.remote, "show", "main:latest.json"))
                                 ["scorecard"][cells.LOGICAL_PATH])
                seen.append(parts)
        self.assertNotEqual(seen[0], seen[1])
        # Only the newest parts commit is reachable from the remote's refs.
        self.assertNotIn(seen[0], git(self.remote, "rev-list", "--all").split())
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("", git(writer, "status", "--porcelain"))
        self.assertEqual(seen[1], git(writer, "rev-parse", "refs/remotes/origin/scorecard-history"))

    def test_a_push_carries_the_history_ref_only_for_an_external_manifest(self):
        writer = self.parent / mirror.WRITER_RELATIVE
        self.append("push-shape")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        head = git(writer, "rev-parse", "HEAD")
        self.assertEqual(["git", "-c", "core.hooksPath=/dev/null", "push", "origin", f"{head}:refs/heads/main"],
                         mirror._push_main_command(writer, head))
        self.assertEqual("published", self.publish()["state"])
        head = git(writer, "rev-parse", "HEAD")
        parts = json.loads(git(writer, "show", f"{head}:{mirror.scorecard_cells.MANIFEST_PATH}"))["parts_commit"]
        self.assertEqual(["git", "-c", "core.hooksPath=/dev/null", "push", "--atomic", "origin",
                          f"{head}:refs/heads/main", f"+{parts}:{mirror.scorecard_cells.HISTORY_REF}"],
                         mirror._push_main_command(writer, head))

    def test_oversized_scorecard_output_is_refused_before_the_writer_changes(self):
        ceiling = mirror.scorecard_cells.FILE_CEILING_BYTES
        self.set_tool(f"(p/'SCORECARD.md').write_bytes(b'x' * {ceiling + 1})\n")
        self.append("oversized")
        report = self.publish()
        self.assertEqual("derived-failed", report["state"])
        self.assertIn(f"scorecard output scorecard/SCORECARD.md exceeds {ceiling} bytes; nothing was copied "
                      "into the ledger writer", report["scorecard"]["error"])
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual(report["series_commit"], git(writer, "rev-parse", "HEAD"))
        self.assertEqual("", git(writer, "status", "--porcelain"))
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        self.assertEqual('{"legacy":"preserve"}', git(self.remote, "show", "main:scorecard/cells.json"))
        self.assertEqual("", git(self.remote, "ls-tree", "main", "--", "scorecard/cells"))
        self.assertEqual([row("oversized")], self.remote_rows())

    def test_a_ledger_storing_both_layouts_is_refused_before_rendering(self):
        seed = self.root / "seed"
        for path, encoded in mirror.scorecard_cells.split(b'{"legacy":"preserve"}\n').items():
            (seed / path).parent.mkdir(parents=True, exist_ok=True)
            (seed / path).write_bytes(encoded)
        git(seed, "add", "scorecard")
        git(seed, "commit", "-qm", "both history layouts")
        git(seed, "push", "-q", "origin", "main")
        marker = self.root / "renderer-ran"
        self.set_tool(f"pathlib.Path({str(marker)!r}).write_text('ran\\n')\n"
                      "(p/'SCORECARD.md').write_text('rendered\\n')\n")
        self.append("both-layouts")
        report = self.publish()
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("both scorecard/cells.json and scorecard/cells/ are present", report["scorecard"]["error"])
        self.assertFalse(marker.exists())
        self.assertEqual([row("both-layouts")], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))

    def test_renderer_writing_history_parts_is_refused_before_push(self):
        # In a copy of a parts commit the renderer sees only the single file;
        # a part it writes is outside its generated output.
        self.append("parts-before-forgery")
        self.assertEqual("published", self.publish()["state"])
        before = self.published_projection()
        self.set_tool("(p/'cells').mkdir()\n"
                      "(p/'cells/part-0000').write_bytes(b'forged')\n"
                      "(p/'SCORECARD.md').write_text('rendered\\n')\n")
        self.append("forged-part")
        report = self.publish()
        self.assertEqual("derived-failed", report["state"])
        self.assertIn("changed ledger paths outside its generated output", report["scorecard"]["error"])
        self.assertIn("scorecard/cells/part-0000", report["scorecard"]["error"])
        self.assertEqual(before, self.published_projection())
        self.assertEqual(b'{"legacy":"preserve"}\n', stored_history(self.remote))
        self.assertIn(row("forged-part"), self.remote_rows())

    def test_parts_view_survives_every_setting_that_would_hide_it(self):
        # Each setting would make Git in the render copy ignore the view (or
        # file it outside refs/replace/), so the renderer would find parts
        # where it reads scorecard/cells.json. The copy's Git never sees them.
        self.append("hidden-view-base")
        self.assertEqual("published", self.publish()["state"])
        config = Path(os.environ["GIT_CONFIG_GLOBAL"])
        user_config = config.read_text()
        system_config = self.root / "system-gitconfig"
        system_config.write_text("[core]\n useReplaceRefs = false\n")
        settings = (
            ("GIT_NO_REPLACE_OBJECTS", {"GIT_NO_REPLACE_OBJECTS": "1"}, ""),
            ("GIT_REPLACE_REF_BASE", {"GIT_REPLACE_REF_BASE": "refs/elsewhere/"}, ""),
            ("user configuration", {}, "[core]\n useReplaceRefs = false\n"),
            ("GIT_CONFIG_COUNT", {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.useReplaceRefs",
                                  "GIT_CONFIG_VALUE_0": "false"}, ""),
            ("system configuration", {"GIT_CONFIG_SYSTEM": str(system_config), "GIT_CONFIG_NOSYSTEM": "0"}, ""),
        )
        for step, (label, variables, configuration) in enumerate(settings):
            with self.subTest(label):
                self.append(f"hidden-view-{step}")
                config.write_text(user_config + configuration)
                try:
                    with mock.patch.dict(os.environ, variables):
                        report = self.publish()
                finally:
                    config.write_text(user_config)
                self.assertEqual("published", report["state"], report)
                self.assertEqual(b'{"legacy":"preserve"}\n', stored_history(self.remote))
                self.assertIn(row(f"hidden-view-{step}"), self.remote_rows())

    def test_renderer_changing_its_copys_replacements_is_refused_before_push(self):
        # The view must stay the copy's only replacement: a renderer that
        # deletes it, or adds one of its own, is refused before anything is
        # copied into the writer.
        self.append("replacements-base")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        edits = (("deleted", "['replace','-d','HEAD']"), ("added", "['replace','--graft','HEAD~1']"))
        for label, arguments in edits:
            with self.subTest(label):
                before = self.published_projection()
                self.set_tool(f"subprocess.run(['git','-C',str(ledger),*{arguments}],check=True)\n"
                              "(p/'SCORECARD.md').write_text('rendered\\n')\n")
                self.append(f"replacement-{label}")
                report = self.publish()
                self.assertEqual("derived-failed", report["state"], report)
                self.assertIn("the ledger copy's replacement objects changed", report["scorecard"]["error"])
                self.assertIn("nothing was copied into the ledger writer", report["scorecard"]["error"])
                self.assertEqual(before, self.published_projection())
                self.assertIn(row(f"replacement-{label}"), self.remote_rows())
                self.assertEqual(report["series_commit"], git(writer, "rev-parse", "HEAD"))
                self.assertEqual("", git(writer, "status", "--porcelain"))

    def forged_view(self, forged: bytes):
        """An install_render_view whose view shows FORGED as the history, not what the commit stores."""
        real_install = mirror.scorecard_cells.install_render_view
        real_git = mirror.scorecard_cells._git

        def forging_git(repo, *args, data=None, **kwargs):
            if args == ("hash-object", "-w", "--stdin"):
                data = forged
            return real_git(repo, *args, data=data, **kwargs)

        def install(repo, commit, environment):
            with mock.patch.object(mirror.scorecard_cells, "_git", side_effect=forging_git):
                return real_install(repo, commit, environment)
        return install

    def test_a_view_showing_other_bytes_is_refused_before_rendering(self):
        # The checked-out bytes are compared first. [forged view]: the view
        # itself shows other bytes, so the `HEAD:scorecard/cells.json` check
        # would refuse it too; the message names the check that refused it.
        # [attributes file]: only the checked-out bytes differ. Git reads a
        # user attributes file even with GIT_CONFIG_GLOBAL=/dev/null, and
        # `text eol=crlf` makes the checkout write CRLF; `git status` compares
        # after the check-in conversion, so the copy is still clean, and the
        # view still shows the stored history. Without the comparison the
        # renderer reads the converted bytes and they are published.
        self.append("forged-view-base")
        self.assertEqual("published", self.publish()["state"])
        with self.subTest("forged view"):
            before = self.published_projection()
            marker = self.root / "renderer-ran"
            self.set_tool(f"pathlib.Path({str(marker)!r}).write_text('ran\\n')\n"
                          "(p/'SCORECARD.md').write_text('rendered\\n')\n")
            self.append("forged-view")
            with mock.patch.object(mirror.scorecard_cells, "install_render_view",
                                   side_effect=self.forged_view(b'{"forged":"view"}\n')):
                report = self.publish()
            self.assertEqual("derived-failed", report["state"], report)
            self.assertIn(f"the ledger copy's checked-out scorecard history differs from {report['series_commit']}",
                          report["scorecard"]["error"])
            self.assertFalse(marker.exists())
            self.assertEqual(before, self.published_projection())
            self.assertIn(row("forged-view"), self.remote_rows())
        with self.subTest("attributes file"):
            history = stored_history(self.remote)
            before = self.published_projection()
            marker = self.root / "converted-renderer-ran"
            attributes = self.root / "xdg" / "git" / "attributes"
            attributes.parent.mkdir(parents=True)
            attributes.write_text(f"{mirror.scorecard_cells.LOGICAL_PATH} text eol=crlf\n")
            self.set_tool(f"pathlib.Path({str(marker)!r}).write_text('ran\\n')\n"
                          "(p/'SCORECARD.md').write_text('rendered\\n')\n")
            self.append("converted-view")
            with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.root / "xdg")}):
                report = self.publish()
            self.assertEqual("derived-failed", report["state"], report)
            self.assertIn(f"the ledger copy's checked-out scorecard history differs from {report['series_commit']}",
                          report["scorecard"]["error"])
            self.assertFalse(marker.exists())
            self.assertEqual(history, stored_history(self.remote))
            self.assertEqual(before, self.published_projection())
            self.assertIn(row("converted-view"), self.remote_rows())

    def test_after_rendering_the_copy_must_still_show_the_stored_history(self):
        # After the render the checked-out file is the renderer's output, so
        # the view is checked through `HEAD:scorecard/cells.json` alone.
        self.append("shown-history-base")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        with mirror._render_copy(writer, base) as (copy, view):
            self.assertIsNotNone(view)
            mirror._require_render_view(copy, base, view, rendered=True)
            environment = mirror._copy_environment()
            subprocess.run(["git", "-C", str(copy), "replace", "-d", base], check=True, capture_output=True,
                           env=environment)
            forged = self.forged_view(b'{"forged":"view"}\n')(copy, base, environment)
            self.assertEqual({base: forged}, mirror.scorecard_cells.replacements(copy, environment))
            with self.assertRaisesRegex(mirror.MirrorRefused,
                                        f"the ledger copy does not show the scorecard history stored at {base}"):
                mirror._require_render_view(copy, base, forged, rendered=True)

    def test_writer_storing_other_bytes_than_generated_is_refused_before_commit(self):
        writer = self.parent / mirror.WRITER_RELATIVE
        cells = mirror.scorecard_cells
        real_replace = cells.replace_tree_external

        def forging_replace(root, manifest, **options):
            # The manifest names a parts commit of other bytes than generated.
            forged = cells.split(b'{"forged":"stored"}\n')
            parts = cells.store_parts_commit(root, forged, environment=mirror._environment())
            return real_replace(root, cells.external_manifest(forged, parts), **options)

        self.append("forged-store")
        with mock.patch.object(cells, "replace_tree_external", side_effect=forging_replace):
            report = self.publish()
        self.assertEqual("derived-failed", report["state"], report)
        self.assertIn("the scorecard history stored in the ledger writer differs from the generated document",
                      report["scorecard"]["error"])
        self.assertEqual(report["series_commit"], git(writer, "rev-parse", "HEAD"))
        # The refused output is kept on a recovery ref, not in the writer:
        # the ref stores exactly the forged history, and the writer is clean.
        self.assertEqual(1, len(report["writer_cleaned"]), report)
        found = re.search(r"saved as (refs/heads/recovery/render-([0-9a-f]{40}))", report["writer_cleaned"][0])
        self.assertIsNotNone(found, report)
        recovery, commit = found.groups()
        self.assertEqual(commit, git(writer, "rev-parse", recovery))
        self.assertEqual([commit, report["series_commit"]],
                         git(writer, "rev-list", "--parents", "-n", "1", commit).split())
        self.assertEqual(b'{"forged":"stored"}\n', mirror.scorecard_cells.read_commit(writer, commit))
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual('{"legacy":"preserve"}', git(self.remote, "show", "main:scorecard/cells.json"))
        self.assertEqual("", git(self.remote, "for-each-ref", "refs/heads/recovery"))
        self.assertEqual("", git(self.remote, "ls-tree", "main", "--", "scorecard/cells"))
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        self.assertEqual([row("forged-store")], self.remote_rows())

    def test_scorecard_head_only_moves_forward_along_ancestry(self):
        self.use_head_renderer()
        root = self.target
        first, _ = self.publish_at(root)
        self.assertEqual("published", first["state"], first)
        latest = self.remote_latest()
        self.assertEqual(root, latest["scorecard_hermit_commit"])
        self.assertEqual(int(git(self.source, "show", "-s", "--format=%ct", root)),
                         latest[mirror.SCORECARD_HEAD_TIME_FIELD])
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        advanced, _ = self.publish_at(child)
        self.assertEqual("published", advanced["scorecard"]["state"], advanced)
        self.assertEqual(child, self.remote_latest()["scorecard_hermit_commit"])
        self.assertEqual(f"rendered at {child}\n", self.remote_file("scorecard/SCORECARD.md"))
        before = self.published_projection()
        self.append("late-older-run")
        with mock.patch.object(mirror, "build_scorecard", wraps=mirror.build_scorecard) as renderer:
            older, stderr = self.publish_at(root)
        renderer.assert_not_called()
        self.assertEqual("published", older["state"], older)
        self.assertEqual({"state": "superseded", "hermit_commit": root, "current_hermit_commit": child},
                         {key: older["scorecard"][key] for key in ("state", "hermit_commit", "current_hermit_commit")})
        self.assertIn("ancestor of the published head", older["scorecard"]["detail"])
        self.assertIn("SCORECARD SUPERSEDED", stderr)
        self.assertEqual(before, self.published_projection())
        self.assertIn(row("late-older-run"), self.remote_rows())
        self.assertEqual(older["commit"], git(self.remote, "rev-parse", "main"))

    def test_divergent_scorecard_heads_order_by_committer_time(self):
        self.use_head_renderer()
        root = self.target
        middle = self.source_commit("branch middle", "2030-01-01T00:00:00Z", base=root)
        older = self.source_commit("branch older", "2029-01-01T00:00:00Z", base=root)
        newer = self.source_commit("branch newer", "2031-01-01T00:00:00Z", base=root)
        tied = self.source_commit("branch tied", "2031-01-01T00:00:00Z", base=root)
        self.assertEqual("published", self.publish_at(middle)[0]["scorecard"]["state"])
        for commit, expected in ((older, "superseded"), (newer, "published"), (tied, "superseded")):
            with self.subTest(commit=commit):
                head = self.remote_latest()["scorecard_hermit_commit"]
                report, _ = self.publish_at(commit)
                self.assertEqual(expected, report["scorecard"]["state"], report)
                self.assertEqual(head if expected == "superseded" else commit,
                                 self.remote_latest()["scorecard_hermit_commit"])
                if expected == "superseded":
                    self.assertIn("divergent history", report["scorecard"]["detail"])
        self.assertEqual(f"rendered at {newer}\n", self.remote_file("scorecard/SCORECARD.md"))

    def metadata_repository(self, head: str, recorded_time=None) -> Path:
        repository = self.root / f"metadata-{head[:8]}-{recorded_time}"
        subprocess.run(["git", "init", "-q", str(repository)], check=True)
        value = {"schema": mirror.METADATA_SCHEMA, "scorecard_hermit_commit": head}
        if recorded_time is not None:
            value[mirror.SCORECARD_HEAD_TIME_FIELD] = recorded_time
        (repository / "latest.json").write_text(json.dumps(value))
        git(repository, "add", "latest.json")
        git(repository, "commit", "-qm", "metadata")
        return repository

    def test_unreadable_published_head_is_ordered_only_by_its_recorded_time(self):
        target_time = int(git(self.source, "show", "-s", "--format=%ct", self.target))
        unknown = "f" * 40
        for recorded, expected in ((target_time - 1, "advance"), (target_time, "superseded"),
                                   (target_time + 1, "superseded")):
            with self.subTest(recorded=recorded):
                order = mirror.scorecard_order(self.metadata_repository(unknown, recorded), self.source, self.target)
                self.assertEqual(expected, order["state"])
                self.assertEqual(unknown, order["current"])
        with self.assertRaisesRegex(mirror.MirrorRefused, "records no scorecard_hermit_commit_time"):
            mirror.scorecard_order(self.metadata_repository(unknown), self.source, self.target)
        # A readable head whose recorded time disagrees with the commit is refused.
        with self.assertRaisesRegex(mirror.MirrorRefused, "records commit time"):
            mirror.scorecard_order(self.metadata_repository(self.target, target_time + 5), self.source, self.target)
        # A shallow measured checkout cannot decide ancestry.
        shallow = self.root / "shallow-source"
        self.source_commit("second", "2030-01-01T00:00:00Z")
        subprocess.run(["git", "clone", "-q", "--depth", "1", f"file://{self.source}", str(shallow)], check=True)
        with self.assertRaisesRegex(mirror.MirrorRefused, "shallow"):
            mirror.scorecard_order(self.metadata_repository(self.target), shallow, git(shallow, "rev-parse", "HEAD"))

    def test_write_metadata_refuses_every_backward_or_stale_order(self):
        self.use_head_renderer()
        self.publish_at(self.target)
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        writer = self.parent / mirror.WRITER_RELATIVE
        snapshot = mirror._published_snapshot(writer)
        order = mirror.scorecard_order(writer, self.source, child)
        self.assertEqual("advance", order["state"])
        refused = (({**order, "state": "superseded"}, child, "may not move"),
                   (order, self.source_commit("other", "2031-01-01T00:00:00Z"), "may not move"),
                   ({**order, "current": "e" * 40}, child, "changed after it was ordered"))
        for bad, target, message in refused:
            with self.subTest(message=message, target=target), self.assertRaisesRegex(mirror.MirrorRefused, message):
                mirror.write_metadata(writer, snapshot, target, snapshot["source"]["commit"], {}, "9" * 40, bad)
        self.assertEqual("", git(writer, "status", "--porcelain"))
        mirror.write_metadata(writer, snapshot, child, snapshot["source"]["commit"], {}, "9" * 40, order)
        written = json.loads((writer / "latest.json").read_text())
        self.assertEqual(child, written["scorecard_hermit_commit"])
        self.assertEqual(order["hermit_commit_time"], written[mirror.SCORECARD_HEAD_TIME_FIELD])
        git(writer, "checkout", "--", "latest.json")

    def proof_doubles(self, prepared, encoded, calls=None):
        selected = prepared["selection"]

        def native(value, parent, results, tool_checkout, **kwargs):
            if calls is not None:
                calls.append(tool_checkout)
            value["artifacts"], value["objects"] = mirror.finalized_proof.capture_derived_cells(selected["row"], encoded)
            return git(self.tool, "rev-parse", "HEAD")

        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(mirror, "finalized_producer_row",
                                              return_value=mirror._row_bytes(selected["row"])))
        stack.enter_context(mock.patch.object(mirror, "prepare_portable_proof", return_value=prepared))
        stack.enter_context(mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40))
        stack.enter_context(mock.patch.object(mirror, "_select_published_terminal",
                                              return_value=(mock.MagicMock(), selected)))
        stack.enter_context(mock.patch.object(mirror, "emit_derived_portable_proof", side_effect=native))
        return stack

    def carrier_paths(self) -> list[str]:
        listed = git(self.remote, "ls-tree", "-r", "--name-only", "main", "--", "scorecard/finalized/runs")
        return listed.splitlines()

    def test_failed_scorecard_still_publishes_the_derived_proof_and_stays_failed(self):
        # The renderer writes, then fails: its partial output is discarded with
        # its ledger copy, and only the carrier is committed beside the rows.
        self.set_tool("(p/'SCORECARD.md').write_text('partial render\\n')\nsys.exit('renderer exploded')\n")
        prepared, encoded = self.derived_fixture()
        calls = []
        self.append("proof-despite-scorecard")
        with self.proof_doubles(prepared, encoded, calls):
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual("failed", report["scorecard"]["state"])
        self.assertIn("renderer exploded", report["scorecard"]["error"])
        proof = report["portable_proof"]
        self.assertEqual("published", proof["state"], proof)
        self.assertEqual([None], calls, "the derived census uses the parent's pinned provider")
        self.assertEqual(report["commit"], proof["commit"])
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        carriers = self.carrier_paths()
        self.assertEqual(1, len(carriers))
        self.assertIn(carriers[0], proof["files"])
        carrier = json.loads(self.remote_file(carriers[0]))
        self.assertEqual(prepared["selection"]["terminal"], carrier["terminal"])
        self.assertEqual(report["series_commit"], carrier["series"]["commit"])
        self.assertIsNone(self.remote_latest())
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertEqual([row("proof-despite-scorecard")], self.remote_rows())
        self.assertIn("SCORECARD FAILED", stderr)
        self.assertIn("FINALIZED PROOF PUBLISHED", stderr)
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("legacy scorecard\n", (writer / "scorecard/SCORECARD.md").read_text())
        self.assertEqual({}, mirror._worktree_changes(writer))

    def test_a_refused_derived_census_skips_the_scorecard_render(self):
        # devbig030, 2026-10-03: an rr-compat-only validate rendered for 30m17s
        # before its derived census was refused, which withholds the scorecard.
        # The census is emitted first now, so the renderer never starts.
        marker = self.root / "renderer-started"
        self.set_tool(f"pathlib.Path({str(marker)!r}).write_text('started\\n')\n"
                      "(p/'SCORECARD.md').write_text('rendered\\n')\n")
        prepared, encoded = self.derived_fixture()
        refusal = "derived census requires an original terminal clean schema-5 full or label run"
        self.append("refused-census")
        with self.proof_doubles(prepared, encoded), \
                mock.patch.object(mirror, "emit_derived_portable_proof",
                                  side_effect=mirror.MirrorRefused(refusal)) as emit:
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertFalse(marker.exists(), "the renderer ran for a census that was refused")
        self.assertEqual(1, emit.call_count)
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual({"state": "failed", "error": refusal}, report["scorecard"])
        self.assertEqual({"state": "failed", "error": refusal}, report["portable_proof"])
        self.assertIn(f"SCORECARD NOT RENDERED: the derived census was refused, which withholds "
                      f"the scorecard: {refusal}", stderr)
        # The rows still publish; nothing else moves.
        self.assertEqual(report["series_commit"], git(self.remote, "rev-parse", "main"))
        self.assertEqual([row("refused-census")], self.remote_rows())
        self.assertIsNone(self.remote_latest())
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertEqual([], self.carrier_paths())
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertNotIn("writer_cleaned", report)
        # The same run with an accepted census renders, emitting it once.
        calls = []
        self.append("accepted-census")
        with self.proof_doubles(prepared, encoded, calls):
            report, _ = self.publish_at(self.target, results=self.root / "results",
                                        finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertTrue(marker.exists())
        self.assertEqual([None], calls)
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertEqual("published", report["state"], report)
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        carriers = self.carrier_paths()
        self.assertEqual(1, len(carriers))
        self.assertIn(carriers[0], report["scorecard"]["files"])

    def test_failed_scorecard_cannot_leave_files_where_the_carrier_is_written(self):
        # A forged carrier in the renderer's own copy is discarded with it.
        self.set_tool("(p/'finalized/runs').mkdir(parents=True)\n"
                      "(p/'finalized/runs/forged.json').write_text('{}\\n')\nsys.exit('renderer exploded')\n")
        prepared, encoded = self.derived_fixture()
        self.append("forged-in-copy")
        with self.proof_doubles(prepared, encoded):
            report, _ = self.publish_at(self.target, results=self.root / "results",
                                        finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertEqual("derived-failed", report["state"])
        self.assertEqual("published", report["portable_proof"]["state"], report)
        carriers = self.carrier_paths()
        self.assertEqual(1, len(carriers))
        self.assertNotIn("scorecard/finalized/runs/forged.json", carriers)
        # One planted in the real writer (reached past the copy) blocks the carrier.
        writer = self.parent / mirror.WRITER_RELATIVE
        self.set_tool(f"w=pathlib.Path({str(writer)!r})/'scorecard/finalized/runs'\n"
                      "w.mkdir(parents=True, exist_ok=True)\n"
                      "(w/'forged.json').write_text('{}\\n')\nsys.exit('renderer exploded')\n")
        again, encoded_again = self.derived_fixture(run_id="another-run")
        again["selection"]["terminal"]["event_id"] = "another-terminal"
        self.append("forged-in-writer")
        with self.proof_doubles(again, encoded_again):
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id="another-run")
        self.assertEqual("derived-failed", report["state"])
        self.assertEqual("failed", report["portable_proof"]["state"])
        self.assertIn("under the finalized proof directory", report["portable_proof"]["error"])
        self.assertEqual(carriers, self.carrier_paths())
        self.assertIn("finalized proof failed", stderr)
        self.assertEqual({"scorecard/finalized/runs/forged.json": "??"}, mirror._worktree_changes(writer))

    def test_superseded_run_publishes_its_derived_proof_without_moving_the_head(self):
        self.use_head_renderer()
        root = self.target
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        self.publish_at(child)
        before = {path: git(self.remote, "rev-parse", "main:" + path)
                  for path in ("latest.json", "scorecard/SCORECARD.md")}
        self.target = root
        prepared, encoded = self.derived_fixture()
        self.append("superseded-with-proof")
        finished_with_rows = []
        actual_finish = mirror.finish_portable_proof

        def finish(prepared, parent, writer, snapshot, *args, **kwargs):
            finished_with_rows.append(snapshot["rows"])
            return actual_finish(prepared, parent, writer, snapshot, *args, **kwargs)

        with self.proof_doubles(prepared, encoded), \
                mock.patch.object(mirror, "finish_portable_proof", side_effect=finish):
            report, _ = self.publish_at(root, results=self.root / "results",
                                        finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertEqual("published", report["state"], report)
        self.assertEqual("superseded", report["scorecard"]["state"])
        self.assertEqual([None], finished_with_rows)
        self.assertEqual("published", report["portable_proof"]["state"])
        self.assertEqual(1, len(self.carrier_paths()))
        self.assertEqual(before, {path: git(self.remote, "rev-parse", "main:" + path) for path in before})
        # The derived census's tool is the carrier tool the receipt names.
        receipt, why = mirror.read_completion_receipt(self.parent, prepared["selection"]["row"]["run_id"])
        self.assertEqual(mirror.SUPERSEDED_RECEIPT_SCHEMA, (receipt or {}).get("schema"), why)
        self.assertEqual(git(self.tool, "rev-parse", "HEAD"), receipt["tool_hermit_commit"])
        self.assertEqual(report["commit"], receipt["carrier_commit"])

    def test_unbound_proof_only_attaches_without_touching_any_scorecard(self):
        # No scorecard was ever published: latest.json does not exist.
        self.append("never-scored")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        prepared, encoded = self.derived_fixture(commit_anchored=True, tree_dirty=False)
        run_id = prepared["selection"]["row"]["run_id"]
        with self.proof_doubles(prepared, encoded), \
                mock.patch.object(mirror, "build_scorecard") as scorecard:
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id=run_id, portable_proof_only=True)
        scorecard.assert_not_called()
        self.assertEqual("published", report["state"], report)
        self.assertEqual({"state": "unbound", "latest_hermit_commit": None,
                          "commit": report["series_commit"], "retry_of": None}, report["existing_scorecard"])
        self.assertEqual("published", report["portable_proof"]["state"])
        self.assertEqual(report["commit"], report["portable_proof"]["commit"])
        carriers = self.carrier_paths()
        self.assertEqual(1, len(carriers))
        self.assertIsNone(self.remote_latest())
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertIn("without a scorecard", stderr)
        published = git(self.remote, "rev-parse", "main")
        files = {path: self.remote_file(path) for path in report["portable_proof"]["files"]}

        def attach(prepared_again, encoded_again, emitted=None):
            with self.proof_doubles(prepared_again, encoded_again, emitted):
                return self.publish_at(self.target, results=self.root / "results",
                                       finalized_run_id=prepared_again["selection"]["row"]["run_id"],
                                       portable_proof_only=True)

        # An exact retry of the same terminal reproduces the published carrier
        # byte for byte and reports unchanged; nothing is pushed.
        retry, retry_stderr = attach(*self.derived_fixture(commit_anchored=True, tree_dirty=False))
        self.assertEqual("unchanged", retry["state"], retry)
        self.assertEqual("unchanged", retry["portable_proof"]["state"], retry)
        self.assertEqual(carriers[0], retry["existing_scorecard"]["retry_of"])
        self.assertEqual(published, retry["portable_proof"]["commit"])
        self.assertEqual(report["portable_proof"]["files"], retry["portable_proof"]["files"])
        self.assertEqual(published, git(self.remote, "rev-parse", "main"))
        self.assertEqual(files, {path: self.remote_file(path) for path in files})
        self.assertIn("FINALIZED PROOF UNCHANGED", retry_stderr)
        # The same terminal with different operands is refused, as is a second
        # carrier for the same run or for the same event under another run.
        changed, changed_encoded = self.derived_fixture(commit_anchored=True, tree_dirty=False)
        changed_encoded = changed_encoded.replace(b'"b' + b"b" * 63 + b'"', b'"c' + b"c" * 63 + b'"')
        refusals = (("changed operands", (changed, changed_encoded), "retry changes the same terminal or proof operands", 1),
                    ("same event", self.derived_fixture(commit_anchored=True, tree_dirty=False, run_id="another-run"),
                     "refusing a second carrier", 0))
        for label, (again, encoded_again), message, emissions in refusals:
            with self.subTest(label):
                emitted = []
                refused, _ = attach(again, encoded_again, emitted)
                self.assertEqual("derived-failed", refused["state"])
                self.assertIn(message, refused["portable_proof"]["error"])
                self.assertEqual(emissions, len(emitted))
                self.assertEqual(published, git(self.remote, "rev-parse", "main"))
                self.assertEqual(carriers, self.carrier_paths())
        other_event, other_encoded = self.derived_fixture(commit_anchored=True, tree_dirty=False)
        other_event["selection"]["terminal"]["event_id"] = "another-terminal"
        emitted = []
        refused, _ = attach(other_event, other_encoded, emitted)
        self.assertIn("refusing a second carrier for run", refused["portable_proof"]["error"])
        self.assertEqual([], emitted)
        self.assertEqual(published, git(self.remote, "rev-parse", "main"))

    # https://github.com/rrnewton/dev-hermit/issues/394: a schema-10 run's
    # carrier used to publish only inside a regenerated scorecard, so every
    # failed or superseded render lost the run's history on the website.

    def remote_subject(self) -> str:
        return git(self.remote, "log", "-1", "--format=%s", "main")

    def test_failed_scorecard_still_publishes_the_schema10_proof_and_stays_failed(self):
        self.set_tool("(p/'SCORECARD.md').write_text('partial render\\n')\nsys.exit('strict native refusal')\n")
        prepared = self.schema10_prepared()
        self.append("schema10-despite-scorecard")
        with self.schema10_proof_doubles(prepared):
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual("failed", report["scorecard"]["state"])
        self.assertIn("strict native refusal", report["scorecard"]["error"])
        proof = report["portable_proof"]
        self.assertEqual("published", proof["state"], proof)
        # The operands were produced by the measured run, not by a tool.
        self.assertEqual(self.target, proof["tool_hermit_commit"])
        self.assertEqual(report["commit"], proof["commit"])
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        self.assertEqual(mirror.PROOF_FALLBACK_COMMIT_MESSAGE, self.remote_subject())
        carriers = self.carrier_paths()
        self.assertEqual(1, len(carriers))
        self.assertIn(carriers[0], proof["files"])
        carrier = json.loads(self.remote_file(carriers[0]))
        self.assertEqual(prepared["selection"]["terminal"], carrier["terminal"])
        self.assertEqual(prepared["artifacts"], carrier["artifacts"])
        self.assertEqual(1, carrier["artifacts"]["schema"])
        self.assertEqual(self.target, carrier["producer"]["hermit_tool_commit"])
        self.assertEqual(report["series_commit"], carrier["series"]["commit"])
        for path, encoded in prepared["objects"].items():
            self.assertEqual(encoded.decode(), self.remote_file(path))
        self.assertIsNone(self.remote_latest())
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertEqual([row("schema10-despite-scorecard")], self.remote_rows())
        self.assertIn("SCORECARD FAILED", stderr)
        self.assertIn("FINALIZED PROOF PUBLISHED", stderr)
        self.assertEqual({}, mirror._worktree_changes(self.parent / mirror.WRITER_RELATIVE))

    def test_superseded_run_publishes_its_schema10_proof_without_moving_the_head(self):
        self.use_head_renderer()
        root = self.target
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        self.publish_at(child)
        before = {path: git(self.remote, "rev-parse", "main:" + path)
                  for path in ("latest.json", "scorecard/SCORECARD.md")}
        self.target = root
        prepared = self.schema10_prepared()
        self.append("superseded-schema10")
        with self.schema10_proof_doubles(prepared):
            report, _ = self.publish_at(root, results=self.root / "results",
                                        finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertEqual("published", report["state"], report)
        self.assertEqual("superseded", report["scorecard"]["state"])
        self.assertEqual("published", report["portable_proof"]["state"], report)
        self.assertEqual(mirror.PROOF_SUPERSEDED_COMMIT_MESSAGE, self.remote_subject())
        self.assertEqual(1, len(self.carrier_paths()))
        self.assertEqual(before, {path: git(self.remote, "rev-parse", "main:" + path) for path in before})

    def test_superseded_run_records_its_outcome_so_later_callers_adopt_it(self):
        # Run validate-claude-coord-mega-lander-66378ba1dd58-1791159507711278531-3607394-20b60ab5
        # (2026-10-05 UTC): the payload's publisher found the scorecard
        # superseded and published the carrier at bfb2e671b9b2, then recorded
        # nothing ("no completion receipt"), so the unit's ExecStopPost ran
        # this whole publisher again for about 588 s until its deadline killed
        # it, and the launcher ran it a third time to find the carrier unchanged.
        self.use_head_renderer()
        root = self.target
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        self.publish_at(child)
        self.target = root
        prepared = self.schema10_prepared()
        run_id = prepared["selection"]["row"]["run_id"]
        finalized_row = mirror._row_bytes(prepared["selection"]["row"])
        self.append("superseded-recorded")
        with self.schema10_proof_doubles(prepared):
            report, stderr = self.publish_at(root, results=self.root / "results", finalized_run_id=run_id)
        self.assertEqual("superseded", report["scorecard"]["state"], report)
        self.assertEqual("published", report["portable_proof"]["state"], report)
        expected = mirror.superseded_receipt(
            run_id=run_id, target=root, current=child, tool_commit=root, carrier_commit=report["commit"],
            series_commit=report["series_commit"], finalized_row=finalized_row,
            files=report["portable_proof"]["files"])
        receipt, why = mirror.read_completion_receipt(self.parent, run_id)
        self.assertEqual(expected, receipt, why)
        self.assertIn(f"superseded receipt {mirror._receipt_path(self.parent, run_id)} names carrier commit "
                      f"{report['commit']} (scorecard head {child})", stderr)
        outcomes = mirror.ledger_publication_outcome
        with contextlib.redirect_stderr(io.StringIO()) as recorder_stderr:
            recorded = mirror.record_publication_outcome(self.parent, run_id, root, report)
        self.assertEqual(outcomes.outcome_path(self.parent, run_id), recorded, recorder_stderr.getvalue())
        adopted, why = outcomes.adopt(self.parent, run_id, root)
        self.assertIsNotNone(adopted, why)
        self.assertEqual(report, adopted.report)
        self.assertEqual(("superseded", report["commit"]), (adopted.outcome, adopted.transaction))
        # A retry finds the carrier already on main, and records that commit.
        self.append("superseded-retry")
        with self.schema10_proof_doubles(prepared):
            retry, _stderr = self.publish_at(root, results=self.root / "results", finalized_run_id=run_id)
        self.assertEqual("unchanged", retry["portable_proof"]["state"], retry)
        receipt, why = mirror.read_completion_receipt(self.parent, run_id)
        self.assertEqual(retry["commit"], (receipt or {}).get("carrier_commit"), why)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertIsNotNone(mirror.record_publication_outcome(self.parent, run_id, root, retry))
        adopted, why = outcomes.adopt(self.parent, run_id, root)
        self.assertEqual(retry, adopted and adopted.report, why)

    def test_a_superseded_receipt_binds_only_the_carrier_files_on_main(self):
        self.use_head_renderer()
        root = self.target
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        self.publish_at(child)
        self.target = root
        prepared = self.schema10_prepared()
        run_id = prepared["selection"]["row"]["run_id"]
        self.append("superseded-bound")
        with self.schema10_proof_doubles(prepared):
            report, _stderr = self.publish_at(root, results=self.root / "results", finalized_run_id=run_id)
        writer = self.parent / mirror.WRITER_RELATIVE
        files = report["portable_proof"]["files"]
        carrier = next(path for path in files if "/runs/" in path)
        arguments = {"run_id": run_id, "target": root, "current": child, "tool_commit": root,
                     "carrier_commit": report["commit"], "series_commit": report["series_commit"],
                     "finalized_row": b"{}\n", "files": files}
        refused = {
            "another digest": ({"files": {**files, carrier: "0" * 64}}, "does not hold carrier file"),
            "a file the commit lacks": ({"files": {**files, "scorecard/finalized/runs/absent.json": "0" * 64}},
                                        "does not hold carrier file"),
            "no file": ({"files": {}}, "names no carrier file"),
            "a commit before the carrier": ({"carrier_commit": git(writer, "rev-parse", f"{report['commit']}^")},
                                            "does not hold carrier file"),
        }
        before = mirror._receipt_path(self.parent, run_id).read_bytes()
        for name, (changes, message) in refused.items():
            with self.subTest(name), self.assertRaisesRegex(mirror.MirrorRefused, message):
                mirror.write_superseded_receipt(self.parent, writer, **{**arguments, **changes})
        with self.assertRaises(mirror.MirrorRefused):
            mirror.write_superseded_receipt(self.parent, writer, **{**arguments, "carrier_commit": "0" * 40})
        self.assertEqual(before, mirror._receipt_path(self.parent, run_id).read_bytes())
        # A receipt that cannot be written is reported; the publication stands.
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            mirror._record_superseded(self.parent, writer, **{**arguments, "files": {}})
        self.assertIn(f"SUPERSEDED RECEIPT NOT WRITTEN: superseded run {run_id} names no carrier file; "
                      f"a later caller of run {run_id} runs the publisher again", stderr.getvalue())

    def test_a_finalized_run_inside_the_regeneration_interval_publishes_rows_and_carrier_unrendered(self):
        # 2026-10-11: 41 regenerations in 24 hours each stored about 1.37 GiB
        # of new scorecard/cells/ parts in the ledger.
        self.enterContext(mock.patch.object(mirror, "SCORECARD_REGENERATION_INTERVAL_SECONDS", 7_200))
        self.use_head_renderer()
        root = self.target
        self.append("rendered-explicitly")
        first, _ = self.publish_at(root)
        self.assertEqual("published", first["scorecard"]["state"], first)
        regenerated_at = int(git(self.remote, "show", "-s", "--format=%ct", first["commit"]))
        before = {path: git(self.remote, "rev-parse", "main:" + path)
                  for path in ("latest.json", "scorecard/SCORECARD.md")}
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        self.target = child
        prepared = self.schema10_prepared()
        run_id = prepared["selection"]["row"]["run_id"]
        finalized_row = mirror._row_bytes(prepared["selection"]["row"])
        self.append("inside-the-interval")
        with self.schema10_proof_doubles(prepared), mock.patch.object(
                mirror, "build_scorecard", side_effect=AssertionError("rendered inside the interval")):
            report, stderr = self.publish_at(child, results=self.root / "results", finalized_run_id=run_id)
        # The rows and the carrier are on main; latest.json and the scorecard are not moved.
        self.assertEqual("published", report["state"], report)
        self.assertEqual("deferred", report["scorecard"]["state"], report)
        self.assertEqual((child, root), (report["scorecard"]["hermit_commit"],
                                         report["scorecard"]["current_hermit_commit"]))
        self.assertIn(row("inside-the-interval"), self.remote_rows())
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        self.assertEqual(mirror.PROOF_DEFERRED_COMMIT_MESSAGE, self.remote_subject())
        self.assertEqual("published", report["portable_proof"]["state"], report)
        self.assertEqual(1, len(self.carrier_paths()))
        self.assertEqual(before, {path: git(self.remote, "rev-parse", "main:" + path) for path in before})
        self.assertIn("SCORECARD DEFERRED: the scorecard was last regenerated", stderr)
        # The outcome is complete: its receipt is recorded and the later callers adopt it.
        expected = mirror.deferred_receipt(
            run_id=run_id, target=child, current=root, tool_commit=child, carrier_commit=report["commit"],
            series_commit=report["series_commit"], regenerated_at=regenerated_at,
            finalized_row=finalized_row, files=report["portable_proof"]["files"])
        receipt, why = mirror.read_completion_receipt(self.parent, run_id)
        self.assertEqual(expected, receipt, why)
        outcomes = mirror.ledger_publication_outcome
        with contextlib.redirect_stderr(io.StringIO()) as recorder_stderr:
            recorded = mirror.record_publication_outcome(self.parent, run_id, child, report)
        self.assertEqual(outcomes.outcome_path(self.parent, run_id), recorded, recorder_stderr.getvalue())
        adopted, why = outcomes.adopt(self.parent, run_id, child)
        self.assertEqual(("deferred", report["commit"]), (adopted.outcome, adopted.transaction), why)
        self.assertEqual(report, adopted.report)
        # A render asked for explicitly (no finalized run) is never deferred.
        self.append("explicit-inside-the-interval")
        explicit, _ = self.publish_at(child)
        self.assertEqual("published", explicit["scorecard"]["state"], explicit)
        self.assertEqual(child, self.remote_latest()["scorecard_hermit_commit"])
        # Once the interval has passed, a finalized run renders again.
        later = self.schema10_prepared(run_id="later-run", record_id="later-record")
        later["selection"]["terminal"].update(run_id="later-run", record_id="later-record",
                                              event_id="later-terminal")
        self.append("after-the-interval")
        actual = mirror.last_regeneration_time
        with self.schema10_proof_doubles(later), mock.patch.object(
                mirror, "last_regeneration_time", side_effect=lambda writer, commit: actual(writer, commit) - 7_200):
            rendered, _ = self.publish_at(child, results=self.root / "results", finalized_run_id="later-run")
        self.assertEqual("published", rendered["scorecard"]["state"], rendered)
        self.assertEqual(mirror.SCORECARD_COMMIT_MESSAGE, self.remote_subject())
        self.assertIn(row("after-the-interval"), self.remote_rows())

    def test_regeneration_deferral_counts_from_the_last_commit_that_moved_latest_json(self):
        writer = self.parent / mirror.WRITER_RELATIVE
        self.append("no-scorecard-yet")
        published = self.publish(publish_only=True)
        with mock.patch.object(mirror, "SCORECARD_REGENERATION_INTERVAL_SECONDS", 7_200):
            # Nothing was ever regenerated: nothing to defer.
            self.assertIsNone(mirror.regeneration_deferral(writer, published["commit"]))
        self.append("first-render")
        first = self.publish()
        regenerated = int(git(writer, "show", "-s", "--format=%ct", first["commit"]))
        # A rows-only commit after the render does not restart the interval.
        self.append("rows-after-the-render")
        rows = self.publish(publish_only=True)
        self.assertNotEqual(first["commit"], rows["commit"])
        self.assertEqual(regenerated, mirror.last_regeneration_time(writer, rows["commit"]))
        with mock.patch.object(mirror, "SCORECARD_REGENERATION_INTERVAL_SECONDS", 7_200):
            self.assertEqual({"regenerated_at": regenerated, "elapsed": 7_199, "interval": 7_200},
                             mirror.regeneration_deferral(writer, rows["commit"], now=regenerated + 7_199))
            self.assertIsNone(mirror.regeneration_deferral(writer, rows["commit"], now=regenerated + 7_200))
            # A regeneration stamped in the future by another host's clock defers nothing.
            self.assertIsNone(mirror.regeneration_deferral(writer, rows["commit"], now=regenerated - 1))
        self.assertIsNone(mirror.regeneration_deferral(writer, rows["commit"], now=regenerated))

    # -- coalesced scorecard renders (ci-hub/lib/scorecard_coalesce.py) ------

    BURST = ("validate-burst-a", "validate-burst-b", "validate-burst-c")

    def finalized(self, run_id: str):
        prepared = self.schema10_prepared(run_id=run_id, record_id=f"{run_id}-record")
        prepared["selection"]["terminal"].update(run_id=run_id, record_id=f"{run_id}-record",
                                                 event_id=f"{run_id}-terminal")
        return prepared

    def counting_renders(self) -> list:
        renders = []
        actual = mirror.build_scorecard

        def counted(*args, **kwargs):
            renders.append(kwargs.get("finalized_run_id"))
            return actual(*args, **kwargs)
        self.enterContext(mock.patch.object(mirror, "build_scorecard", side_effect=counted))
        return renders

    def publish_finalized(self, run_id: str, *, coalesce: bool, commit: str | None = None):
        with self.schema10_proof_doubles(self.finalized(run_id)):
            return self.publish_at(commit or self.target, results=self.root / "results",
                                   finalized_run_id=run_id, coalesce_scorecard=coalesce)

    def burst(self, *, coalesce: bool) -> dict:
        """Three finalized publications of one measured commit, queued A, B, C behind each other.

        Coalescing, each holder sees the later ones waiting for
        publication.lock (their waiter files, as publish_deferred_unit writes
        them); serialized, nobody waits.
        """
        self.use_head_renderer()
        root = self.target
        self.append("before-the-burst")
        first, _ = self.publish_at(root)
        self.assertEqual("published", first["scorecard"]["state"], first)
        renders = self.counting_renders()
        coalesce_module = mirror.scorecard_coalesce
        waiting = {run_id: coalesce_module.register_waiter(self.parent, run_id=run_id, target=root)
                   for run_id in self.BURST[1:]} if coalesce else {}
        reports = {}
        for run_id in self.BURST:
            # This publication now holds publication.lock: it waits no longer.
            coalesce_module.unregister_waiter(waiting.pop(run_id, None))
            self.append(f"{run_id}-row")
            reports[run_id], _stderr = self.publish_finalized(run_id, coalesce=coalesce)
        latest = self.remote_latest()
        return {"renders": renders, "reports": reports, "latest": latest,
                "scorecard_md": self.remote_file("scorecard/SCORECARD.md"),
                "rows_rendered": git(self.remote, "show",
                                     f"{latest['source_commit']}:series/hermit/fixture/2026-09.jsonl")}

    def fresh_fixture_at_fixed_dates(self) -> None:
        """A new ledger, measured source and tool whose commits do not depend on the clock."""
        # One date for both fixtures, taken now: the coalescing cap reads the
        # last regeneration's commit time.
        if not hasattr(self, "fixed_date"):
            self.fixed_date = f"@{int(time.time())} +0000"
        self.enterContext(mock.patch.dict(os.environ, {"GIT_AUTHOR_DATE": self.fixed_date,
                                                       "GIT_COMMITTER_DATE": self.fixed_date}))
        self.setUp()

    @staticmethod
    def rendered_projection(state: dict) -> dict:
        """What a render produces from the rows: everything but the commits the ledger history names."""
        latest = state["latest"]
        bound = {path.as_posix() for path in (*mirror.MIRRORED_SCORECARD_PATHS, *mirror.OPTIONAL_SCORECARD_PATHS)}
        return {"latest": {key: value for key, value in latest.items()
                           if key not in {"source_commit", "series_commit", "scorecard"}},
                "files": {path: digest for path, digest in latest["scorecard"].items() if path in bound},
                "scorecard_md": state["scorecard_md"]}

    def test_a_burst_of_queued_publications_renders_once_and_matches_serialized_renders(self):
        self.fresh_fixture_at_fixed_dates()
        coalesced = self.burst(coalesce=True)
        a, b, c = self.BURST
        # Exactly one render, by the last publication of the burst.
        self.assertEqual([c], coalesced["renders"])
        reports = coalesced["reports"]
        for run_id in (a, b):
            scorecard = reports[run_id]["scorecard"]
            self.assertEqual(("published", "deferred", "queued"),
                             (reports[run_id]["state"], scorecard["state"], scorecard["reason"]), reports[run_id])
            self.assertEqual(self.target, scorecard["current_hermit_commit"])
            self.assertIn("is queued on publication.lock", scorecard["detail"])
            self.assertEqual("published", reports[run_id]["portable_proof"]["state"])
        self.assertIn(f"publication {b} is queued", reports[a]["scorecard"]["detail"])
        self.assertIn(f"publication {c} is queued", reports[b]["scorecard"]["detail"])
        self.assertEqual("published", reports[c]["scorecard"]["state"], reports[c])
        # The deferred runs' rows are counted in the final scorecard.
        for run_id in self.BURST:
            self.assertIn(f'"{run_id}-row"', coalesced["rows_rendered"])
        self.assertEqual(len(coalesced["rows_rendered"].splitlines()), coalesced["latest"]["rows"])
        # Their pick-up markers name the render that covered them.
        self.assertEqual([a, b], reports[c]["covered_deferrals"])
        for run_id in (a, b):
            marker = mirror.scorecard_coalesce.read_deferral(self.parent, run_id)
            self.assertEqual(reports[c]["commit"], marker["covered_by"]["transaction"], marker)
            self.assertEqual(reports[run_id]["series_commit"], marker["series_commit"])
        self.assertEqual([], mirror.scorecard_coalesce.pending(self.parent))
        # The deferred receipt names the reason; later callers adopt the outcome.
        receipt, why = mirror.read_completion_receipt(self.parent, a)
        self.assertEqual(("deferred", "queued"), (receipt["outcome"], receipt["reason"]), why)
        outcomes = mirror.ledger_publication_outcome
        with contextlib.redirect_stderr(io.StringIO()):
            mirror.record_publication_outcome(self.parent, a, self.target, reports[a])
        adopted, why = outcomes.adopt(self.parent, a, self.target)
        self.assertEqual(("deferred", reports[a]["commit"]), (adopted.outcome, adopted.transaction), why)
        # The same three publications rendered one after another.
        self.fresh_fixture_at_fixed_dates()
        serialized = self.burst(coalesce=False)
        self.assertEqual(list(self.BURST), serialized["renders"])
        self.assertEqual(self.rendered_projection(serialized), self.rendered_projection(coalesced))
        self.assertEqual(serialized["rows_rendered"], coalesced["rows_rendered"])

    def test_the_coalesced_scorecard_keeps_its_commit_binding(self):
        # S3: latest.json binds the files it was rendered from; coalescing
        # changes who renders, never that check.
        self.burst(coalesce=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        snapshot = mirror._published_snapshot(writer)
        mirror.published_scorecard_binding(writer, snapshot, self.target)
        metadata = json.loads((writer / "latest.json").read_bytes())
        history = mirror.scorecard_cells.LOGICAL_PATH
        self.assertEqual(hashlib.sha256(mirror._scorecard_bytes(writer, history)).hexdigest(),
                         metadata["scorecard"][history])
        mismatched = {
            "a history digest it was not rendered from": dict(
                metadata, scorecard={**metadata["scorecard"], history: "0" * 64}),
            "another row digest": dict(metadata, rows_sha256="0" * 64),
            "another series tree": dict(metadata, source_tree="0" * 40),
        }
        for name, value in mismatched.items():
            (writer / "latest.json").write_text(json.dumps(value))
            with self.subTest(name), self.assertRaises(mirror.MirrorRefused):
                mirror.published_scorecard_binding(writer, snapshot, self.target)
        (writer / "latest.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")
        mirror.published_scorecard_binding(writer, snapshot, self.target)

    def test_an_orphaned_deferral_is_covered_by_the_backlog_render(self):
        # The waiter dies before it renders: nothing in the burst covers the
        # deferred run, so publish_backlog renders as the nightly job does
        # (no --finalized-run-id, never deferred) and that render covers it.
        self.use_head_renderer()
        root = self.target
        self.append("before")
        self.publish_at(root)
        renders = self.counting_renders()
        waiter = mirror.scorecard_coalesce.register_waiter(self.parent, run_id="validate-dies", target=root)
        self.append("deferred-row")
        report, _ = self.publish_finalized("validate-orphan", coalesce=True)
        self.assertEqual(("deferred", "queued"), (report["scorecard"]["state"], report["scorecard"]["reason"]))
        self.assertEqual([], renders)
        mirror.scorecard_coalesce.unregister_waiter(waiter)  # it died without publishing
        self.assertEqual(["validate-orphan"],
                         [value["run_id"] for _path, value in mirror.scorecard_coalesce.pending(self.parent)])
        pickup, _ = self.publish_at(root)
        self.assertEqual("published", pickup["scorecard"]["state"], pickup)
        self.assertEqual([None], renders)
        self.assertEqual(["validate-orphan"], pickup["covered_deferrals"])
        self.assertEqual([], mirror.scorecard_coalesce.pending(self.parent))
        rendered = git(self.remote, "show", f"{self.remote_latest()['source_commit']}:series/hermit/fixture/2026-09.jsonl")
        self.assertIn('"deferred-row"', rendered)

    def test_unknown_or_unprovable_waiters_render_now(self):
        self.use_head_renderer()
        root = self.target
        self.append("first")
        self.publish_at(root)
        # The finalized runs measure a descendant of the published head.
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        renders = self.counting_renders()
        waiters = self.parent / mirror.scorecard_coalesce.WAITERS_RELATIVE
        register = mirror.scorecard_coalesce.register_waiter
        cases = {
            "an unreadable waiter file": lambda: (waiters.mkdir(parents=True, exist_ok=True),
                                                  (waiters / "validate-garbage.json").write_text("{")),
            "a waiter of a commit this checkout does not have": lambda: register(
                self.parent, run_id="validate-unknown", target="9" * 40),
            "a waiter with no measured commit": lambda: register(
                self.parent, run_id="validate-untargeted", target=None),
            # Serialized, this run's render would supersede it, so it never
            # renders after this one.
            "a waiter of an ancestor of this commit": lambda: register(
                self.parent, run_id="validate-older", target=root),
        }
        for index, (name, arrange) in enumerate(cases.items()):
            with self.subTest(name):
                arrange()
                run_id = f"validate-failsafe-{index}"
                self.append(f"{run_id}-row")
                report, _ = self.publish_finalized(run_id, coalesce=True, commit=child)
                self.assertEqual("published", report["scorecard"]["state"], report)
                self.assertEqual(run_id, renders[-1])
                self.assertIsNone(mirror.scorecard_coalesce.read_deferral(self.parent, run_id))
        # A live waiter whose deferral marker cannot be written: render.
        register(self.parent, run_id="validate-live", target=child)
        markers = self.parent / mirror.scorecard_coalesce.DEFERRALS_RELATIVE
        if os.geteuid() != 0:
            markers.mkdir(parents=True, exist_ok=True)
            markers.chmod(0o500)
            self.append("no-marker-row")
            report, stderr = self.publish_finalized("validate-no-marker", coalesce=True, commit=child)
            markers.chmod(0o755)
            self.assertEqual("published", report["scorecard"]["state"], report)
            self.assertIn("cannot be recorded", stderr)
            markers.rmdir()
        # A marker store that cannot be read: the cap cannot be checked, so render.
        markers.parent.mkdir(parents=True, exist_ok=True)
        markers.write_text("a file, not a directory")
        self.append("unreadable-markers-row")
        report, stderr = self.publish_finalized("validate-unreadable-markers", coalesce=True, commit=child)
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertIn("the pending deferrals cannot be read", stderr)
        markers.unlink()
        # Without --coalesce-scorecard (the validation payload's own call, a
        # launcher) a live waiter changes nothing.
        self.append("not-coalescing-row")
        report, _ = self.publish_finalized("validate-not-coalescing", coalesce=False, commit=child)
        self.assertEqual("published", report["scorecard"]["state"], report)
        # With it, the same live waiter defers.
        self.append("coalescing-row")
        report, _ = self.publish_finalized("validate-coalescing", coalesce=True, commit=child)
        self.assertEqual(("deferred", "queued"), (report["scorecard"]["state"], report["scorecard"].get("reason")))

    def test_an_ever_present_waiter_cannot_keep_the_scorecard_stale_past_the_cap(self):
        # coord2 condition (i): under continuous load every holder sees a
        # waiter, so only a cap read from durable timestamps bounds staleness.
        self.use_head_renderer()
        root = self.target
        self.append("before")
        self.publish_at(root)
        renders = self.counting_renders()
        coalesce = mirror.scorecard_coalesce
        coalesce.register_waiter(self.parent, run_id="validate-forever", target=root)
        cap = mirror.coalesce_staleness_cap_seconds()
        self.assertEqual(mirror.SCORECARD_REGENERATION_INTERVAL_SECONDS + mirror.COALESCE_CAP_MARGIN_SECONDS, cap)
        for index in range(2):
            self.append(f"steady-{index}")
            report, _ = self.publish_finalized(f"validate-steady-{index}", coalesce=True)
            self.assertEqual(("deferred", "queued"), (report["scorecard"]["state"], report["scorecard"].get("reason")))
        self.assertEqual([], renders)
        # The last regeneration reaches the cap: this holder renders although
        # the waiter is still there, and covers the deferred runs.
        actual = mirror.last_regeneration_time
        self.append("capped")
        with mock.patch.object(mirror, "last_regeneration_time",
                               side_effect=lambda writer, commit: actual(writer, commit) - cap):
            report, stderr = self.publish_finalized("validate-capped", coalesce=True)
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertEqual(["validate-capped"], renders)
        self.assertIn("rendering although publication validate-forever is queued", stderr)
        self.assertIn("coalescing cap", stderr)
        self.assertEqual(["validate-steady-0", "validate-steady-1"], report["covered_deferrals"])
        # Just inside the cap it still defers.
        self.append("inside")
        with mock.patch.object(mirror, "last_regeneration_time",
                               side_effect=lambda writer, commit: actual(writer, commit) - cap + 60):
            report, _ = self.publish_finalized("validate-inside", coalesce=True)
        self.assertEqual("deferred", report["scorecard"]["state"], report)
        # A pending deferral that has waited the cap forces a render too,
        # whatever the last regeneration says (another host may have rendered
        # without its rows).
        marker = coalesce.deferral_path(self.parent, "validate-inside")
        value = json.loads(marker.read_text())
        marker.write_text(json.dumps({**value, "deferred_at_epoch": value["deferred_at_epoch"] - cap}))
        self.append("old-marker")
        report, stderr = self.publish_finalized("validate-old-marker", coalesce=True)
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertEqual("validate-old-marker", renders[-1])
        self.assertIn("the deferral of validate-inside has waited", stderr)
        # The staleness bound: the cap, one deferring holder, one rendering holder.
        self.assertEqual(cap + 2 * mirror.PUBLICATION_HOLD_SECONDS, mirror.queued_deferral_staleness_bound_seconds())
        with mock.patch.object(mirror, "SCORECARD_REGENERATION_INTERVAL_SECONDS", 7_200):
            self.assertEqual(10_800, mirror.coalesce_staleness_cap_seconds())
            self.assertEqual(16_800, mirror.queued_deferral_staleness_bound_seconds())

    def test_one_holds_bound_is_start_units_publication_unit(self):
        validate = str(mirror.TOOL_ROOT / "ci-hub/validate")
        sys.path.insert(0, validate)
        self.addCleanup(sys.path.remove, validate)
        import start_unit
        self.assertEqual(start_unit.PUBLICATION_STARTUP_RESERVE_SECONDS + start_unit.LEDGER_MIRROR_LIFECYCLE_SECONDS
                         + start_unit.PUBLICATION_TAIL_SECONDS, mirror.PUBLICATION_HOLD_SECONDS)
        self.assertEqual(mirror.LIFECYCLE_TIMEOUT_SECONDS, start_unit.LEDGER_MIRROR_LIFECYCLE_SECONDS)

    def test_the_coalescing_cap_boundary_and_unreadable_state(self):
        self.append("rendered")
        self.publish()
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        regenerated = mirror.last_regeneration_time(writer, head)
        cap = mirror.coalesce_staleness_cap_seconds()
        capped = mirror._coalescing_capped
        self.assertIsNone(capped(self.parent, writer, head, now=regenerated + cap - 1))
        self.assertIn("at or past", capped(self.parent, writer, head, now=regenerated + cap) or "")
        # A pending deferral's age has the same boundary.
        coalesce = mirror.scorecard_coalesce
        coalesce.record_deferral(self.parent, run_id="validate-waiting", target=self.target,
                                 series_commit=head, waiter={})
        marker = coalesce.deferral_path(self.parent, "validate-waiting")
        marker.write_text(json.dumps({**json.loads(marker.read_text()), "deferred_at_epoch": regenerated - 10}))
        self.assertIsNone(capped(self.parent, writer, head, now=regenerated + cap - 11))
        self.assertIn("validate-waiting has waited", capped(self.parent, writer, head, now=regenerated + cap - 10) or "")
        marker.write_text(json.dumps({**json.loads(marker.read_text()), "deferred_at_epoch": "yesterday"}))
        self.assertIn("records no deferral time", capped(self.parent, writer, head, now=regenerated) or "")
        marker.unlink()
        # A regeneration time that cannot be read caps (renders), never defers.
        with mock.patch.object(mirror, "last_regeneration_time", side_effect=mirror.MirrorRefused("unreadable")):
            self.assertIn("cannot be read: unreadable", capped(self.parent, writer, head, now=regenerated) or "")
        with mock.patch.object(mirror, "last_regeneration_time", return_value=None):
            self.assertIn("no scorecard regeneration", capped(self.parent, writer, head, now=regenerated) or "")

    def test_renders_after_follows_serialized_head_order(self):
        # A waiter is deferred to only if serialized renders would end with its
        # render: scorecard_order lets it replace this commit as the head.
        root = self.target
        target = self.source_commit("target", "2030-01-01T00:00:00Z", base=root)
        newer = self.source_commit("divergent, newer", "2031-01-01T00:00:00Z", base=root)
        older = self.source_commit("divergent, older", "2029-01-01T00:00:00Z", base=root)
        same_time = self.source_commit("divergent, same time", "2030-01-01T00:00:00Z", base=root)
        descendant = self.source_commit("descendant, older time", "2001-01-01T00:00:00Z", base=target)
        ancestor = self.source_commit("ancestor, newer time", "2035-01-01T00:00:00Z", base=root)
        child = self.source_commit("child of that ancestor", "2032-01-01T00:00:00Z", base=ancestor)
        after = mirror._renders_after
        self.assertEqual("the queued publication measured this commit", after(self.source, target, target))
        self.assertIn("divergent commit with a newer committer time", after(self.source, target, newer) or "")
        self.assertIsNone(after(self.source, target, older))
        self.assertIsNone(after(self.source, target, same_time), "only a strictly newer time replaces the head")
        self.assertIn("descendant", after(self.source, target, descendant) or "")
        self.assertIsNone(after(self.source, child, ancestor), "an ancestor never replaces its descendant")
        self.assertIsNone(after(self.source, target, None))
        self.assertIsNone(after(self.source, target, "9" * 40))

    def test_coalescing_is_refused_outside_an_ordinary_finalized_publication(self):
        for kwargs in ({}, {"publish_only": True}, {"scorecard_command": "export-legacy"}):
            with self.subTest(kwargs), self.assertRaisesRegex(mirror.MirrorRefused, "--coalesce-scorecard"):
                self.publish(coalesce_scorecard=True, **kwargs)
        parser = mirror.build_parser()
        self.assertTrue(parser.parse_args(["--parent", "/p", "--coalesce-scorecard"]).coalesce_scorecard)
        self.assertFalse(parser.parse_args(["--parent", "/p"]).coalesce_scorecard)

    def test_unbound_proof_only_attaches_a_schema10_carrier_and_a_retry_is_unchanged(self):
        # The recovery pass in ci-hub/ledger/publish_backlog.py runs exactly this.
        self.append("schema10-never-scored")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        prepared = self.schema10_prepared(commit_anchored=True, tree_dirty=False)
        run_id = prepared["selection"]["row"]["run_id"]
        with self.schema10_proof_doubles(prepared), \
                mock.patch.object(mirror, "build_scorecard") as scorecard:
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id=run_id, portable_proof_only=True)
        scorecard.assert_not_called()
        self.assertEqual("published", report["state"], report)
        self.assertEqual("unbound", report["existing_scorecard"]["state"])
        self.assertIsNone(report["existing_scorecard"]["retry_of"])
        self.assertEqual("published", report["portable_proof"]["state"])
        self.assertEqual(self.target, report["portable_proof"]["tool_hermit_commit"])
        self.assertEqual(mirror.PROOF_COMMIT_MESSAGE, self.remote_subject())
        carriers = self.carrier_paths()
        self.assertEqual(1, len(carriers))
        self.assertIsNone(self.remote_latest())
        self.assertIn("without a scorecard", stderr)
        published = git(self.remote, "rev-parse", "main")
        files = {path: self.remote_file(path) for path in report["portable_proof"]["files"]}
        again = self.schema10_prepared(commit_anchored=True, tree_dirty=False)
        with self.schema10_proof_doubles(again):
            retry, retry_stderr = self.publish_at(self.target, results=self.root / "results",
                                                  finalized_run_id=run_id, portable_proof_only=True)
        self.assertEqual("unchanged", retry["state"], retry)
        self.assertEqual("unchanged", retry["portable_proof"]["state"], retry)
        self.assertEqual(carriers[0], retry["existing_scorecard"]["retry_of"])
        self.assertEqual(published, git(self.remote, "rev-parse", "main"))
        self.assertEqual(files, {path: self.remote_file(path) for path in files})
        self.assertIn("FINALIZED PROOF UNCHANGED", retry_stderr)

    def test_unbound_retry_refuses_to_complete_a_published_carrier(self):
        # A retry may only reproduce a complete published carrier; it never
        # adds a file the published carrier is missing.
        self.append("incomplete")
        self.publish(publish_only=True)
        prepared, encoded = self.derived_fixture(commit_anchored=True, tree_dirty=False)
        with self.proof_doubles(prepared, encoded):
            first, _ = self.publish_at(self.target, results=self.root / "results",
                                       finalized_run_id=prepared["selection"]["row"]["run_id"],
                                       portable_proof_only=True)
        self.assertEqual("published", first["portable_proof"]["state"], first)
        objects = [path for path in first["portable_proof"]["files"] if "/objects/" in path]
        self.assertEqual(1, len(objects))
        clone = self.root / "remove-object"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(clone)], check=True)
        git(clone, "rm", "-q", objects[0])
        git(clone, "commit", "-qm", "fixture: drop the carrier object")
        git(clone, "push", "-q", "origin", "HEAD:main")
        before = git(self.remote, "rev-parse", "main")
        again, encoded_again = self.derived_fixture(commit_anchored=True, tree_dirty=False)
        with self.proof_doubles(again, encoded_again):
            refused, _ = self.publish_at(self.target, results=self.root / "results",
                                         finalized_run_id=again["selection"]["row"]["run_id"],
                                         portable_proof_only=True)
        self.assertEqual("derived-failed", refused["state"], refused)
        self.assertIn("the published carrier is incomplete", refused["portable_proof"]["error"])
        self.assertIn(objects[0], refused["portable_proof"]["error"])
        self.assertEqual(before, git(self.remote, "rev-parse", "main"))

    def test_unbound_proof_only_leaves_another_commits_scorecard_in_place(self):
        root = self.target
        self.append("scored-elsewhere")
        self.assertEqual("published", self.publish_at(root)[0]["state"])
        before = self.published_projection()
        self.target = self.source_commit("unscored run", "2030-01-01T00:00:00Z")
        prepared, encoded = self.derived_fixture(commit_anchored=True, tree_dirty=False)
        with self.proof_doubles(prepared, encoded):
            report, _ = self.publish_at(self.target, results=self.root / "results",
                                        finalized_run_id=prepared["selection"]["row"]["run_id"],
                                        portable_proof_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual(root, report["existing_scorecard"]["latest_hermit_commit"])
        self.assertEqual(self.target, report["portable_proof"]["hermit_commit"])
        self.assertEqual(before, self.published_projection())
        self.assertEqual(1, len(self.carrier_paths()))

    def test_unbound_proof_only_requires_a_commit_anchored_clean_run(self):
        self.append("unqualified")
        self.publish(publish_only=True)
        for label, updates in (("not anchored", {"commit_anchored": None}),
                               ("anchor false", {"commit_anchored": False}),
                               ("dirty tree", {"commit_anchored": True, "tree_dirty": True}),
                               ("dirty unknown", {"commit_anchored": True, "tree_dirty": None})):
            with self.subTest(label):
                prepared, encoded = self.derived_fixture(**updates)
                emitted = []
                with self.proof_doubles(prepared, encoded, emitted):
                    report, _ = self.publish_at(self.target, results=self.root / "results",
                                                finalized_run_id=prepared["selection"]["row"]["run_id"],
                                                portable_proof_only=True)
                self.assertEqual("derived-failed", report["state"])
                self.assertIn("requires a commit-anchored run of a clean tree", report["portable_proof"]["error"])
                self.assertEqual([], emitted)
                self.assertEqual([], self.carrier_paths())


    # --- Round-2 review: https://github.com/rrnewton/dev-hermit/issues/394 ---

    def test_renderer_commits_in_the_real_writer_never_reach_the_remote(self):
        # F1: a renderer that reaches past its copy and commits in the real
        # writer must not ride underneath the derived carrier's commit, whether
        # it then fails or claims success.
        writer = self.parent / mirror.WRITER_RELATIVE
        forged_latest = json.dumps({"schema": mirror.METADATA_SCHEMA, "scorecard_hermit_commit": "f" * 40,
                                    mirror.SCORECARD_HEAD_TIME_FIELD: 4102444800}) + "\n"
        forgeries = {
            "empty commit": "",
            "forged latest.json": f"(w/'latest.json').write_text({forged_latest!r})\n",
            "forged carrier": "(w/'scorecard/finalized/runs').mkdir(parents=True, exist_ok=True)\n"
                              "(w/'scorecard/finalized/runs/forged.json').write_text('{}\\n')\n",
        }
        number = 0
        for label, forge in forgeries.items():
            for exploded in (True, False):
                number += 1
                with self.subTest(label, exploded=exploded):
                    ending = ("sys.exit('renderer exploded')\n" if exploded
                              else "(p/'SCORECARD.md').write_text('rendered\\n')\n")
                    self.set_tool(f"w=pathlib.Path({str(writer)!r})\n" + forge
                                  + "subprocess.run(['git','-C',str(w),'add','-A'],check=True)\n"
                                  + f"subprocess.run(['git','-C',str(w),'commit','--allow-empty','-qm',"
                                  + f"'renderer forged {number}'],check=True)\n" + ending)
                    prepared, encoded = self.derived_fixture(run_id=f"forging-run-{number}")
                    prepared["selection"]["terminal"]["event_id"] = f"forging-terminal-{number}"
                    self.append(f"forged-{number}")
                    with self.proof_doubles(prepared, encoded):
                        report, stderr = self.publish_at(self.target, results=self.root / "results",
                                                         finalized_run_id=f"forging-run-{number}")
                    self.assertEqual("derived-failed", report["state"], report)
                    self.assertEqual("failed", report["scorecard"]["state"])
                    if not exploded:
                        self.assertIn("the ledger writer HEAD moved while the scorecard tool ran",
                                      report["scorecard"]["error"])
                    self.assertEqual("failed", report["portable_proof"]["state"], report)
                    self.assertIn(f"is not the published series commit {report['series_commit']}",
                                  report["portable_proof"]["error"])
                    self.assertIn("writer retained for recovery", report["portable_proof"]["error"])
                    # The remote holds this run's rows and nothing else.
                    self.assertEqual(report["series_commit"], git(self.remote, "rev-parse", "main"))
                    self.assertIn(row(f"forged-{number}"), self.remote_rows())
                    self.assertNotIn("renderer forged", git(self.remote, "log", "--all", "--format=%s"))
                    self.assertIsNone(self.remote_latest())
                    self.assertEqual([], self.carrier_paths())
                    self.assertIn("SCORECARD FAILED", stderr)
                    # The forged commit is retained in the writer, directly on the series commit.
                    forged = git(writer, "rev-parse", "HEAD")
                    self.assertEqual(f"renderer forged {number}", git(writer, "log", "-1", "--format=%s", forged))
                    self.assertEqual([forged, report["series_commit"]],
                                     git(writer, "rev-list", "--parents", "-n", "1", forged).split())
                    # Operator recovery for the next subtest: drop the forged commit.
                    git(writer, "checkout", "-q", "--detach", report["series_commit"])

    def test_renderer_writes_in_the_real_writer_withhold_the_scorecard(self):
        # F1/F4: a renderer that claims success after writing past its copy
        # into the real writer's files (without committing) publishes no
        # scorecard, even where its write would be overwritten by the copy;
        # the derived carrier still publishes and the write is kept for recovery.
        writer = self.parent / mirror.WRITER_RELATIVE
        legacy = self.remote_file("scorecard/SCORECARD.md")
        self.assertEqual("legacy scorecard\n", legacy)
        # A write to a path the render itself produces is set aside on a
        # recovery ref by the end of the run (its bytes are checked there);
        # any other write is not render output and stays in the writer.
        writes = {
            "stray file": ("stray.txt", "(w/'stray.txt').write_text('stray\\n')\n", "??", False),
            "generated path": ("scorecard/SCORECARD.md",
                               "(w/'scorecard/SCORECARD.md').write_text('forged in the writer\\n')\n", "M", True),
        }
        for number, (label, (path, forge, status, rescued)) in enumerate(writes.items(), start=1):
            with self.subTest(label):
                self.set_tool(f"w=pathlib.Path({str(writer)!r})\n" + forge
                              + "(p/'SCORECARD.md').write_text('rendered\\n')\n")
                prepared, encoded = self.derived_fixture(run_id=f"writing-run-{number}")
                prepared["selection"]["terminal"]["event_id"] = f"writing-terminal-{number}"
                before = self.carrier_paths()
                self.append(f"writing-{number}")
                with self.proof_doubles(prepared, encoded):
                    report, stderr = self.publish_at(self.target, results=self.root / "results",
                                                     finalized_run_id=f"writing-run-{number}")
                self.assertEqual("derived-failed", report["state"], report)
                self.assertEqual("failed", report["scorecard"]["state"])
                self.assertIn("the ledger writer changed while the scorecard tool ran; kept for recovery "
                              "(render outputs under refs/heads/recovery/render-*, anything else in the "
                              f"writer): {path}", report["scorecard"]["error"])
                self.assertEqual("published", report["portable_proof"]["state"], report)
                self.assertEqual(report["portable_proof"]["commit"], git(self.remote, "rev-parse", "main"))
                self.assertEqual(1, len(self.carrier_paths()) - len(before))
                # Neither the write nor the rendered scorecard reached the remote.
                self.assertIsNone(self.remote_latest())
                self.assertEqual(legacy, self.remote_file("scorecard/SCORECARD.md"))
                self.assertIsNone(self.remote_file("stray.txt"))
                self.assertIn("SCORECARD FAILED", stderr)
                self.assertEqual("", git(self.remote, "for-each-ref", "refs/heads/recovery"))
                if rescued:
                    self.assertEqual(1, len(report["writer_cleaned"]), report)
                    self.assertIn(f"left {path} in the ledger writer", stderr)
                    found = re.search(r"saved as (refs/heads/recovery/render-([0-9a-f]{40}))",
                                      report["writer_cleaned"][0])
                    self.assertIsNotNone(found, report)
                    recovery, commit = found.groups()
                    self.assertEqual([commit, report["portable_proof"]["commit"]],
                                     git(writer, "rev-list", "--parents", "-n", "1", recovery).split())
                    self.assertEqual([path], git(writer, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                                 f"{recovery}^", recovery).splitlines())
                    self.assertEqual("forged in the writer", git(writer, "show", f"{recovery}:{path}"))
                    self.assertEqual({}, mirror._worktree_changes(writer))
                else:
                    self.assertNotIn("writer_cleaned", report)
                    self.assertNotIn("WRITER CLEANED", stderr)
                    self.assertEqual({path: status}, {key: value.strip() for key, value
                                                      in mirror._worktree_changes(writer).items()})
                # Operator recovery for the next subtest.
                git(writer, "checkout", "-q", "--", ".")
                git(writer, "clean", "-qfd")

    def test_commit_and_push_only_pushes_one_commit_directly_on_its_base(self):
        # F1 (second line of defence): every publication commit is checked
        # against the base it was computed from before anything is pushed.
        self.append("commit-base")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        self.assertEqual(base, git(self.remote, "rev-parse", "main"))
        real_git = mirror._git

        def attempt(name, prepare=None, during_commit=None):
            (writer / name).write_text(name + "\n")
            if prepare is not None:
                prepare()

            def interfering(repo, *args, **kwargs):
                if during_commit is not None and "commit" in args:
                    during_commit()
                return real_git(repo, *args, **kwargs)

            with mock.patch.object(mirror, "_git", side_effect=interfering):
                return mirror._commit_and_push(writer, [name], "fixture publication",
                                               net_wrapper="/usr/bin/env", base=base)

        def stage(name):
            (writer / name).write_text("smuggled\n")
            git(writer, "add", name)

        refusals = (
            ("moved head", {"prepare": lambda: git(writer, "commit", "--allow-empty", "-qm", "forged underneath")},
             "is not the publication base"),
            ("extra parent", {"during_commit": lambda: git(writer, "update-ref", "HEAD", git(
                writer, "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "raced underneath"))},
             "does not sit directly on the publication base"),
            ("extra path", {"during_commit": lambda: stage("smuggled-during-commit")},
             "changes paths other than this publication"),
            ("pre-staged path", {"prepare": lambda: stage("smuggled-before")},
             "index holds paths outside this publication"),
        )
        for label, hooks, message in refusals:
            with self.subTest(label):
                with self.assertRaisesRegex(mirror.MirrorRefused, message):
                    attempt(label.replace(" ", "-") + ".txt", **hooks)
                self.assertEqual(base, git(self.remote, "rev-parse", "main"))
                git(writer, "reset", "-q", "--hard", base)
                git(writer, "clean", "-qfd")
        # The push sends one commit to main and nothing else: a replace ref in
        # the writer (the render copy's view is the only one production makes,
        # and never there) is not published, nor is its replacement object.
        replaced = git(writer, "rev-parse", f"{base}^")
        replacement = git(writer, "commit-tree", f"{base}^{{tree}}", "-m", "replacement never published")
        git(writer, "replace", replaced, replacement)
        commit = attempt("qualifying.txt")
        self.assertEqual(commit, git(self.remote, "rev-parse", "main"))
        self.assertEqual([commit, base], git(self.remote, "rev-list", "--parents", "-n", "1", commit).split())
        self.assertEqual("qualifying.txt",
                         git(self.remote, "diff-tree", "--no-commit-id", "--name-only", "-r", base, commit))
        self.assertEqual("refs/heads/main", git(self.remote, "for-each-ref", "--format=%(refname)"))
        missing = subprocess.run(["git", "-C", str(self.remote), "cat-file", "-e", replacement],
                                 capture_output=True, check=False)
        self.assertNotEqual(0, missing.returncode)

    def test_refused_derived_push_sets_the_commit_aside_and_frees_the_writer(self):
        # https://github.com/rrnewton/dev-hermit/issues/447 (devbig030,
        # 2026-10-03): another host published first, the derived push was
        # refused, and the writer stayed on the derived commit, which
        # recover_spooled_commit refuses, so no later run published anything.
        self.append("before-race")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        other = self.root / "other-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        git(other, "commit", "--allow-empty", "-qm", "another host published first")
        git(other, "push", "-q", "origin", "main")
        (writer / "derived.txt").write_text("derived\n")
        (writer / "leftover.txt").write_text("unrelated renderer leftover\n")
        with self.assertRaisesRegex(mirror.MirrorRefused,
                                    f"kept as refs/heads/recovery/derived-[0-9a-f]{{40}}; writer returned to {base}"):
            mirror._commit_and_push(writer, ["derived.txt"], "fixture derived publication",
                                    net_wrapper="/usr/bin/env", base=base)
        self.assertEqual(base, git(writer, "rev-parse", "HEAD"))
        name, commit = git(writer, "for-each-ref", "--format=%(refname) %(objectname)",
                           "refs/heads/recovery/").split()
        self.assertEqual(f"refs/heads/recovery/derived-{commit}", name)
        self.assertEqual([commit, base], git(writer, "rev-list", "--parents", "-n", "1", commit).split())
        self.assertEqual("derived", git(writer, "show", f"{commit}:derived.txt"))
        self.assertFalse((writer / "derived.txt").exists())
        self.assertEqual("unrelated renderer leftover\n", (writer / "leftover.txt").read_text())
        (writer / "leftover.txt").unlink()
        self.append("after-race")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertEqual([row("before-race"), row("after-race")], self.remote_rows())

    def test_a_refused_derived_push_keeps_the_render_record_on_the_head_it_returns_to(self):
        # The record of what a failed scorecard step wrote is keyed by HEAD. A
        # derived commit made afterwards moves HEAD to it and a refused push
        # moves HEAD back; the record must follow both moves, or the next run
        # refuses the step's leftovers as unpublished work and the writer jams
        # as in https://github.com/rrnewton/dev-hermit/issues/447.
        self.append("before-race")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        other = self.root / "other-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        git(other, "commit", "--allow-empty", "-qm", "another host published first")
        git(other, "push", "-q", "origin", "main")
        leftover = b"failed render\n"
        mirror._record_render_provenance(writer, [("scorecard/SCORECARD.md", None),
                                                  ("scorecard/SCORECARD.md", leftover)])
        (writer / "scorecard/SCORECARD.md").write_bytes(leftover)
        (writer / "derived.txt").write_text("derived\n")
        moves = []
        original = mirror._move_render_provenance

        def move(writer_path, old, new, committed=()):
            original(writer_path, old, new, committed)
            moves.append((old, new, mirror._read_render_provenance(writer_path, new)))

        with mock.patch.object(mirror, "_move_render_provenance", side_effect=move), \
                self.assertRaisesRegex(mirror.MirrorRefused, f"writer returned to {base}"):
            mirror._commit_and_push(writer, ["derived.txt"], "fixture derived publication",
                                    net_wrapper="/usr/bin/env", base=base)
        digest = hashlib.sha256(leftover).hexdigest()
        self.assertEqual(2, len(moves), moves)
        self.assertEqual(base, moves[0][0])
        self.assertEqual({"scorecard/SCORECARD.md": {None, digest}}, moves[0][2])
        self.assertEqual((moves[0][1], base), moves[1][:2])
        self.assertEqual({"scorecard/SCORECARD.md": {None, digest}},
                         mirror._read_render_provenance(writer, base))
        self.assertEqual({}, mirror._read_render_provenance(writer, moves[0][1]))
        self.assertEqual(leftover, (writer / "scorecard/SCORECARD.md").read_bytes())
        self.append("after-race")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertRegex(" ".join(report["writer_cleaned"]), "saved as refs/heads/recovery/render-")
        self.assertEqual([row("before-race"), row("after-race")], self.remote_rows())

    def test_a_run_killed_as_a_refused_push_returns_the_writer_keeps_its_render_record(self):
        # Review of a3f62cfee1: a run killed after a refused push's checkout
        # returned HEAD to its base, and before the render record followed it
        # there, left a record naming only the derived commit, and the next
        # run refused the scorecard step's leftovers as unpublished work. The
        # record now holds at both commits before the switch. The kill is the
        # move after the checkout raising a BaseException no handler takes.
        self.append("before-race")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        other = self.root / "other-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        git(other, "commit", "--allow-empty", "-qm", "another host published first")
        git(other, "push", "-q", "origin", "main")
        leftover = b"failed render\n"
        mirror._record_render_provenance(writer, [("scorecard/SCORECARD.md", None),
                                                  ("scorecard/SCORECARD.md", leftover)])
        (writer / "scorecard/SCORECARD.md").write_bytes(leftover)
        (writer / "derived.txt").write_text("derived\n")
        original = mirror._move_render_provenance

        class Killed(BaseException):
            pass

        def move(writer_path, old, new, committed=()):
            if new == base:
                raise Killed
            original(writer_path, old, new, committed)

        with mock.patch.object(mirror, "_move_render_provenance", side_effect=move), self.assertRaises(Killed):
            mirror._commit_and_push(writer, ["derived.txt"], "fixture derived publication",
                                    net_wrapper="/usr/bin/env", base=base)
        self.assertEqual(base, git(writer, "rev-parse", "HEAD"))
        self.assertEqual({"scorecard/SCORECARD.md": {None, hashlib.sha256(leftover).hexdigest()}},
                         mirror._read_render_provenance(writer, base))
        self.assertEqual({"scorecard/SCORECARD.md": " M"}, mirror._worktree_changes(writer))
        self.append("after-race")
        with contextlib.redirect_stderr(io.StringIO()):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertRegex(" ".join(report["writer_cleaned"]), "saved as refs/heads/recovery/render-")
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual([row("before-race"), row("after-race")], self.remote_rows())

    def _strand(self, writer: Path, files: dict[str, str | bytes], message: str, *,
                symlinks: dict[str, str] | None = None, executable: tuple[str, ...] = (),
                removed: tuple[str, ...] = (), commit_args: tuple[str, ...] = ()) -> str:
        """Leave WRITER on one unpushed commit, as a run that died or lost its push did."""
        if removed:
            git(writer, "rm", "-q", "--", *removed)
        for path, content in files.items():
            (writer / path).parent.mkdir(parents=True, exist_ok=True)
            if isinstance(content, bytes):
                (writer / path).write_bytes(content)
            else:
                (writer / path).write_text(content)
        for path, target in (symlinks or {}).items():
            (writer / path).parent.mkdir(parents=True, exist_ok=True)
            os.symlink(target, writer / path)
        for path in executable:
            (writer / path).chmod(0o755)
        paths = [*files, *(symlinks or {}), *executable]
        if paths:
            git(writer, "add", "--", *paths)
        git(writer, "commit", "-q", *commit_args, "-m", message)
        return git(writer, "rev-parse", "HEAD")

    @staticmethod
    def _scorecard_metadata(files: dict[str, str | bytes],
                            history: bytes = b'{"legacy":"preserve"}\n') -> str:
        """latest.json binding FILES and the cells HISTORY by digest, as write_metadata writes it."""
        hashes = {path: hashlib.sha256(content if isinstance(content, bytes) else content.encode()).hexdigest()
                  for path, content in files.items()}
        hashes[mirror.scorecard_cells.LOGICAL_PATH] = hashlib.sha256(history).hexdigest()
        return json.dumps({"schema": mirror.METADATA_SCHEMA, "scorecard": hashes},
                          indent=2, sort_keys=True) + "\n"

    @staticmethod
    def _proof_files(run: str) -> dict[str, bytes]:
        """One valid derived-cells carrier and its object, as a proof attachment adds them."""
        proof = mirror.finalized_proof
        binding = {"run_id": run, "raw_census_sha256": "1" * 64, "history_binding_sha256": "2" * 64}
        cells = proof.canonical_bytes({
            "schema": 1, "authority": proof.DERIVED_AUTHORITY, "record_id": f"{run}-record",
            "hermit_sha": "3" * 40, "attempts": [], **binding}) + b"\n"
        digest = hashlib.sha256(cells).hexdigest()
        manifest = proof.validate_manifest({
            "schema": 1,
            "terminal": {"repository": "rrnewton/hermit", "run_id": run, "record_id": f"{run}-record",
                         "event_id": f"{run}-event", "event_sha256": "4" * 64, "hermit_commit": "3" * 40},
            "parent": {"repository": proof.PARENT_REPOSITORY, "commit": "5" * 40,
                       "shard": "ledger/hermit/fixture/2026-10.jsonl", "blob_oid": "6" * 40,
                       "event_line_sha256": "7" * 64},
            "artifacts": {"schema": 2, "raw_census_sha256": binding["raw_census_sha256"],
                          "history_binding_sha256": binding["history_binding_sha256"],
                          "derived_cells": {"path": proof.derived_path(binding), "bytes": len(cells),
                                            "sha256": digest}},
            "producer": {"parent_commit": "8" * 40, "hermit_tool_commit": "9" * 40},
            "series": {"repository": proof.LEDGER_REPOSITORY, "commit": "a" * 40, "tree": "b" * 40,
                       "rows_sha256": "c" * 64},
        })
        return {proof.carrier_path(manifest): proof.canonical_bytes(manifest) + b"\n",
                proof.object_path(digest): cells}

    @staticmethod
    def _schema1_proof_files(run: str, padding: int) -> dict[str, bytes]:
        """One valid schema-1 carrier and its plan, cells and tests objects, each about PADDING bytes."""
        proof = mirror.finalized_proof
        operands = {
            "plan": (json.dumps({"plan": "p" * padding}) + "\n").encode(),
            "cells": (json.dumps({"cell": "c" * padding}) + "\n").encode(),
            "tests": (json.dumps({"test": "t" * padding}) + "\n").encode(),
        }
        artifacts: dict = {"schema": 1}
        for role, encoded in operands.items():
            artifacts[role] = {"path": f"ignored/validate/artifacts/{run}/{role}.json", "bytes": len(encoded),
                               "sha256": hashlib.sha256(encoded).hexdigest()}
        manifest = proof.validate_manifest({
            "schema": 1,
            "terminal": {"repository": "rrnewton/hermit", "run_id": run, "record_id": f"{run}-record",
                         "event_id": f"{run}-event", "event_sha256": "4" * 64, "hermit_commit": "3" * 40},
            "parent": {"repository": proof.PARENT_REPOSITORY, "commit": "5" * 40,
                       "shard": "ledger/hermit/fixture/2026-10.jsonl", "blob_oid": "6" * 40,
                       "event_line_sha256": "7" * 64},
            "artifacts": artifacts,
            "producer": {"parent_commit": "8" * 40, "hermit_tool_commit": "9" * 40},
            "series": {"repository": proof.LEDGER_REPOSITORY, "commit": "a" * 40, "tree": "b" * 40,
                       "rows_sha256": "c" * 64},
        })
        return {proof.carrier_path(manifest): proof.canonical_bytes(manifest) + b"\n",
                **{proof.object_path(hashlib.sha256(encoded).hexdigest()): encoded
                   for encoded in operands.values()}}

    def test_a_publication_starting_on_a_stranded_derived_commit_sets_it_aside(self):
        # https://github.com/rrnewton/dev-hermit/issues/447, devbig014 RUN 2004,
        # 2026-10-03: a tool tree from before the push-time set-aside lost its
        # proof push to another host's append and left the writer on ea80c0c.
        # Every later publication refused "unpublished derived or unknown work"
        # and the exact-tip validate's retirement was RETIRE-REFUSED.
        self.append("before-strand")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        other = self.root / "other-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        git(other, "commit", "--allow-empty", "-qm", "another host published first")
        git(other, "push", "-q", "origin", "main")
        files = self._proof_files("stranded-run")
        carrier = next(path for path in files if "/runs/" in path)
        stranded = self._strand(writer, files, mirror.PROOF_FALLBACK_COMMIT_MESSAGE)
        self.append("after-strand")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("before-strand"), row("after-strand")], self.remote_rows())
        self.assertEqual(stranded, git(writer, "rev-parse", f"refs/heads/recovery/derived-{stranded}"))
        self.assertEqual(files[carrier].decode().strip(), git(writer, "show", f"{stranded}:{carrier}"))
        self.assertIn(f"ledger publication: WRITER FREED: a previous run left the writer on derived "
                      f"commit {stranded}", stderr.getvalue())
        self.assertIn(f"writer returned to {base}", stderr.getvalue())
        # The derived content is not republished by an unrelated run.
        absent = subprocess.run(["git", "-C", str(self.remote), "cat-file", "-e", f"main:{carrier}"],
                                capture_output=True, check=False)
        self.assertNotEqual(0, absent.returncode)
        # A stranded regenerated scorecard is set aside the same way.
        scorecard = {"scorecard/SCORECARD.md": "stranded\n", **self._proof_files("scorecard-run")}
        stranded = self._strand(writer, {"latest.json": self._scorecard_metadata(scorecard), **scorecard},
                                mirror.SCORECARD_COMMIT_MESSAGE)
        self.append("after-scorecard")
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertEqual(stranded, git(writer, "rev-parse", f"refs/heads/recovery/derived-{stranded}"))
        self.assertEqual([row("before-strand"), row("after-strand"), row("after-scorecard")],
                         self.remote_rows())

    def test_a_scorecard_commit_left_by_a_lost_push_is_freed_by_the_next_run(self):
        # The publisher's own scorecard commit (latest.json, SCORECARD.md and
        # the history moved into parts), left on the writer when its push lost
        # a race, as a tool tree without the push-time set-aside left ea80c0c.
        # The racing commit is not appended data, so the commit is not rebuilt
        # on it (_push_rebuilt_over_appends) and the set-aside runs.
        self.append("before-scorecard")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        other = self.root / "other-host"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        build = mirror.build_scorecard

        def build_then_lose_the_race(*args, **kwargs):
            built = build(*args, **kwargs)
            (other / "operator-note.txt").write_text("not appended data\n")
            git(other, "add", "operator-note.txt")
            git(other, "commit", "-qm", "another host published first")
            git(other, "push", "-q", "origin", "main")
            return built

        with mock.patch.object(mirror, "build_scorecard", side_effect=build_then_lose_the_race), \
                mock.patch.object(mirror, "_set_aside_unpushed_commit",
                                  side_effect=lambda _writer, commit, _base: f"writer retained at {commit}"):
            report = self.publish()
        stranded = git(writer, "rev-parse", "HEAD")
        self.assertEqual("failed", report["scorecard"]["state"], report)
        self.assertIn(f"writer retained at {stranded}", report["scorecard"]["error"])
        self.assertEqual([base], git(writer, "rev-list", "--parents", "-n", "1", stranded).split()[1:])
        self.assertEqual(mirror.SCORECARD_COMMIT_MESSAGE, git(writer, "log", "-1", "--format=%B", stranded))
        self.assertEqual(["A\tlatest.json", "M\tscorecard/SCORECARD.md", "D\tscorecard/cells.json",
                          "A\tscorecard/cells/manifest.json"],
                         git(writer, "diff-tree", "--no-commit-id", "--name-status", "-r",
                             base, stranded).splitlines())
        self.append("after-scorecard")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertIn(f"ledger publication: WRITER FREED: a previous run left the writer on derived "
                      f"commit {stranded}", stderr.getvalue())
        self.assertEqual(stranded, git(writer, "rev-parse", f"refs/heads/recovery/derived-{stranded}"))
        self.assertEqual([row("before-scorecard"), row("after-scorecard")], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))

    # A derived push that lost a race to another host's push is committed
    # again on the new main when that host only appended data
    # (_push_rebuilt_over_appends).

    @staticmethod
    def _write(root: Path, files: dict[str, bytes]) -> None:
        for path, content in files.items():
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_bytes(content)

    @staticmethod
    def _appending(*identifiers: str):
        """An edit appending rows for IDENTIFIERS to the fixture's series file, as the publisher writes them."""
        def edit(other: Path) -> None:
            with (other / "series/hermit/fixture/2026-09.jsonl").open("a") as stream:
                for identifier in identifiers:
                    stream.write(series._canonical_row_json(row(identifier)) + "\n")
        return edit

    @contextlib.contextmanager
    def _another_host_pushes_first(self, *edits):
        """Before each of the next len(EDITS) derived pushes, another host pushes to main.

        Each edit changes the other host's clone of main, which is committed
        and pushed. Yields the other host's commits, in the order they landed.
        """
        other = self.root / "other-host"
        if not other.exists():
            subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        pending, pushed, real = list(edits), [], mirror._network

        def network(command, **kwargs):
            if "push" in command and pending:
                git(other, "pull", "-q", "--ff-only", "origin", "main")
                pending.pop(0)(other)
                git(other, "add", "-A")
                git(other, "commit", "-qm", "another host published first")
                git(other, "push", "-q", "origin", "main")
                pushed.append(git(other, "rev-parse", "HEAD"))
            return real(command, **kwargs)

        with mock.patch.object(mirror, "_network", side_effect=network):
            yield pushed
        self.assertEqual([], pending, "a racing push never happened")

    def _proof_in_writer(self, run: str) -> tuple[Path, str, dict[str, bytes]]:
        """Publish a row, then write RUN's proof in the writer; return the writer, its base and the files."""
        self.append(f"before-{run}")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        files = self._proof_files(run)
        self._write(writer, files)
        return writer, git(writer, "rev-parse", "HEAD"), files

    @staticmethod
    def _push_proof(writer: Path, files: dict[str, bytes], base: str) -> tuple[str | None, str]:
        """Commit and push FILES on BASE as a proof attachment does; return the commit on main and stderr."""
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            published = mirror._commit_and_push(writer, sorted(files), mirror.PROOF_COMMIT_MESSAGE,
                                                net_wrapper="/usr/bin/env", base=base)
        return published, stderr.getvalue()

    def test_a_proof_push_lost_to_another_hosts_rows_is_rebuilt_on_them(self):
        # 2026-10-06: a carrier took about 5 minutes from reading main to its
        # push, other hosts appended rows every 5 to 15 minutes, and one
        # carrier lost three races in a row, set aside each time.
        writer, base, files = self._proof_in_writer("raced-run")
        with self._another_host_pushes_first(self._appending("elsewhere")) as racers:
            published, stderr = self._push_proof(writer, files, base)
        self.assertEqual(1, len(racers))
        self.assertEqual(published, git(self.remote, "rev-parse", "main"))
        self.assertEqual([published, racers[0]],
                         git(self.remote, "rev-list", "--parents", "-n", "1", published).split())
        self.assertEqual(sorted(files), git(self.remote, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                            racers[0], published).splitlines())
        for path, content in files.items():
            self.assertEqual(content, git_bytes(self.remote, "show", f"{published}:{path}"))
        self.assertEqual(mirror.PROOF_COMMIT_MESSAGE, git(self.remote, "log", "-1", "--format=%B", published))
        # The writer moved to the published commit and nothing was set aside.
        self.assertEqual(published, git(writer, "rev-parse", "HEAD"))
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
        self.assertIn("ledger publication: PUSH RACE LOST AND RECOVERED: ", stderr)
        self.assertIn(f"rebuilt as {published} on {racers[0]}, which main reached from {base} by appends "
                      f"only, and on main after 1 rebuild(s); writer moved to {published}", stderr)
        self.assertEqual([row("before-raced-run"), row("elsewhere")], self.remote_rows())
        # The next publication starts from the rebuilt commit.
        self.append("after-race")
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertEqual([row("before-raced-run"), row("elsewhere"), row("after-race")], self.remote_rows())
        subprocess.run(["git", "-C", str(self.remote), "merge-base", "--is-ancestor", published, "main"],
                       check=True)

    def test_a_lost_proof_push_is_set_aside_when_main_moved_by_more_than_appends(self):
        self.append("before-refusals")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE

        def lose(run: str, edit) -> tuple[str, str, str, dict[str, bytes]]:
            """Push RUN's proof from the current main, after another host pushed EDIT; expect a set-aside."""
            git(writer, "fetch", "-q", "origin", "main")
            git(writer, "checkout", "-q", "--detach", "refs/remotes/origin/main")
            base = git(writer, "rev-parse", "HEAD")
            files = self._proof_files(run)
            self._write(writer, files)
            with self._another_host_pushes_first(lambda other: edit(other, files)) as racers, \
                    self.assertRaises(mirror.MirrorRefused) as refused:
                self._push_proof(writer, files, base)
            message = str(refused.exception)
            self.assertRegex(message, rf"kept as refs/heads/recovery/derived-[0-9a-f]{{40}}; "
                                      rf"writer returned to {base}$")
            self.assertEqual(base, git(writer, "rev-parse", "HEAD"))
            self.assertEqual({}, mirror._worktree_changes(writer))
            carrier = next(path for path in files if "/runs/" in path)
            absent = subprocess.run(["git", "-C", str(self.remote), "cat-file", "-e", f"main:{carrier}"],
                                    capture_output=True, check=False)
            self.assertNotEqual(0, absent.returncode)
            return message, base, racers[0], files

        shard = "series/hermit/fixture/2026-09.jsonl"
        written = {
            "latest.json": ({"latest.json": b"{}\n"}, "latest.json (A 000000 -> 100644)"),
            "another scorecard": ({"scorecard/SCORECARD.md": b"another host's scorecard\n"},
                                  "scorecard/SCORECARD.md (M 100644 -> 100644)"),
            "another file": ({"operator-note.txt": b"not appended data\n"},
                             "operator-note.txt (A 000000 -> 100644)"),
        }
        for index, (name, (theirs, change)) in enumerate(written.items()):
            with self.subTest(name):
                message, base, racer, _ = lose(f"refused-{index}",
                                               lambda other, _ours, theirs=theirs: self._write(other, theirs))
                self.assertIn(f"main moved from {base} to {racer} by more than appends: "
                              f"{change} is not appended data", message)
        with self.subTest("a rewritten row"):
            def rewrite(other: Path, _ours) -> None:
                (other / shard).write_bytes((other / shard).read_bytes().replace(b'"passed"', b'"failed"'))

            message, base, racer, _ = lose("refused-rewrite", rewrite)
            self.assertIn(f"main moved from {base} to {racer} by more than appends: "
                          f"{shard} (M 100644 -> 100644) is not appended data", message)
        with self.subTest("another carrier of this run"):
            theirs = self._schema1_proof_files("raced-carrier", 16)
            carrier = next(path for path in theirs if "/runs/" in path)
            message, base, racer, ours = lose("raced-carrier", lambda other, _ours: self._write(other, theirs))
            self.assertNotIn(carrier, ours)
            self.assertIn(f"main moved from {base} to {racer} by more than appends: {carrier} is another "
                          "finalized carrier of run raced-carrier (event raced-carrier-event)", message)
        with self.subTest("other bytes at this proof's object path"):
            def collide(other: Path, ours) -> None:
                self._write(other, {path: b"other bytes\n" for path in ours if "/objects/" in path})

            message, base, racer, ours = lose("refused-collision", collide)
            obj = next(path for path in ours if "/objects/" in path)
            self.assertIn(f"; main {racer} also changed {obj}; ", message)
            self.assertNotIn("by more than appends", message)

    def test_a_rebuilt_proof_leaves_out_an_object_main_already_holds(self):
        # Two runs with the same operand bytes add the same content-named object.
        writer, base, files = self._proof_in_writer("shared-object-run")
        carrier = next(path for path in files if "/runs/" in path)
        obj = next(path for path in files if "/objects/" in path)
        with self._another_host_pushes_first(lambda other: self._write(other, {obj: files[obj]})) as racers:
            published, stderr = self._push_proof(writer, files, base)
        self.assertEqual(published, git(self.remote, "rev-parse", "main"))
        self.assertEqual([obj], git(self.remote, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                    base, racers[0]).splitlines())
        self.assertEqual([carrier], git(self.remote, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                        racers[0], published).splitlines())
        self.assertEqual(files[obj], git_bytes(self.remote, "show", f"{published}:{obj}"))
        # The carrier still binds the object main already held.
        self.assertEqual((racers[0], mirror.PROOF_COMMIT_MESSAGE, 1), mirror.derived_commit_base(writer, published))
        self.assertIn(f"on main after 1 rebuild(s); writer moved to {published}", stderr)

    def test_a_proof_push_that_keeps_losing_is_set_aside_after_the_last_rebuild(self):
        writer, base, files = self._proof_in_writer("outrun-run")
        others = [f"elsewhere-{number}" for number in range(1, mirror.REAPPLY_ATTEMPTS + 2)]
        with self._another_host_pushes_first(*map(self._appending, others)) as racers, \
                self.assertRaises(mirror.MirrorRefused) as refused:
            self._push_proof(writer, files, base)
        message = str(refused.exception)
        # The first push and each rebuild's push lost a race.
        self.assertEqual(mirror.REAPPLY_ATTEMPTS + 1, len(racers))
        self.assertRegex(message, rf"main moved again after {mirror.REAPPLY_ATTEMPTS} rebuilds; "
                                  r"last refusal: [0-9a-f]{40}: ")
        self.assertRegex(message, rf"kept as refs/heads/recovery/derived-[0-9a-f]{{40}}; writer returned to {base}$")
        self.assertEqual(racers[-1], git(self.remote, "rev-parse", "main"))
        self.assertEqual(base, git(writer, "rev-parse", "HEAD"))
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual([row(identifier) for identifier in ("before-outrun-run", *others)], self.remote_rows())

    def test_a_refused_proof_push_is_not_rebuilt_when_main_did_not_move(self):
        writer, base, files = self._proof_in_writer("refused-run")
        real, pushes = mirror._network, []

        def network(command, **kwargs):
            if "push" in command:
                pushes.append(command)
                if len(pushes) == 1:
                    raise mirror.MirrorRefused("git failed: fixture: push refused")
            return real(command, **kwargs)

        with mock.patch.object(mirror, "_network", side_effect=network), \
                self.assertRaises(mirror.MirrorRefused) as refused:
            self._push_proof(writer, files, base)
        self.assertIn(f"git failed: fixture: push refused; main is still at {base}, so the push was not "
                      "refused because main moved; commit ", str(refused.exception))
        self.assertEqual(1, len(pushes))
        self.assertEqual(base, git(writer, "rev-parse", "HEAD"))
        self.assertEqual(base, git(self.remote, "rev-parse", "main"))

    def test_a_proof_push_whose_check_failed_is_found_on_main(self):
        writer, base, files = self._proof_in_writer("confirmed-run")
        real, calls = mirror._network, []

        def network(command, **kwargs):
            calls.append("push" if "push" in command else "fetch" if "fetch" in command else command[0])
            if len(calls) == 2:
                raise mirror.MirrorRefused("git failed: fixture: fetch interrupted")
            return real(command, **kwargs)

        with mock.patch.object(mirror, "_network", side_effect=network):
            commit, stderr = self._push_proof(writer, files, base)
        self.assertEqual(["push", "fetch", "fetch"], calls)
        self.assertEqual(commit, git(self.remote, "rev-parse", "main"))
        self.assertEqual(commit, git(writer, "rev-parse", "HEAD"))
        self.assertEqual([commit, base], git(self.remote, "rev-list", "--parents", "-n", "1", commit).split())
        self.assertIn(f"ledger publication: PUSH CONFIRMED ON A SECOND READ: git failed: fixture: fetch "
                      f"interrupted; {commit} is on main after all", stderr)
        self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))

    def _render_leftovers(self, writer: Path) -> dict[str, str]:
        return {path: status for path, status in mirror._worktree_changes(writer).items()}

    def _assert_rescued(self, writer: Path, outcome: str, head: str, saved: dict[str, bytes | None]) -> None:
        """OUTCOME names a recovery ref on HEAD storing SAVED exactly; the writer is clean."""
        self.assertIn(f"writer cleaned at {head}", outcome)
        found = re.search(r"saved as (refs/heads/recovery/render-([0-9a-f]{40}))", outcome)
        self.assertIsNotNone(found, outcome)
        recovery, commit = found.groups()
        self.assertEqual(commit, git(writer, "rev-parse", recovery))
        self.assertEqual([commit, head], git(writer, "rev-list", "--parents", "-n", "1", commit).split())
        self.assertEqual(sorted(saved), git(writer, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                            head, commit).splitlines())
        for path, content in saved.items():
            stored = subprocess.run(["git", "-C", str(writer), "cat-file", "blob", f"{commit}:{path}"],
                                    capture_output=True, check=False)
            if content is None:
                self.assertNotEqual(0, stored.returncode, path)
            else:
                self.assertEqual(content, stored.stdout, path)
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertFalse((writer / "scorecard/cells").exists())
        self.assertEqual('{"legacy":"preserve"}\n', (writer / "scorecard/cells.json").read_text())

    def _snapshot_leftovers(self, writer: Path) -> dict[str, bytes | None]:
        return {path: None if status.strip() == "D" else (writer / path).read_bytes()
                for path, status in mirror._worktree_changes(writer).items()}

    def test_a_render_killed_while_it_runs_never_dirties_the_writer(self):
        # The renderer writes only in its throwaway copy, so SIGKILL in the
        # middle of a render (after it has started writing) leaves the writer
        # exactly at the published series commit and the next run renders.
        self.append("before-kill")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        self.set_tool("(p/'SCORECARD.md').write_text('half a scorecard')\n"
                      "(p/'parity.json').write_text('{')\n"
                      "import signal\nos.kill(os.getpid(), signal.SIGKILL)\n")
        self.append("during-kill")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish()
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual("failed", report["scorecard"]["state"])
        self.assertIn("rust-script failed", report["scorecard"]["error"])
        self.assertEqual(report["series_commit"], git(writer, "rev-parse", "HEAD"))
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertNotIn("WRITER CLEANED", stderr.getvalue())
        self.assertNotIn("writer_cleaned", report)
        self.assertEqual([row("before-kill"), row("during-kill")], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        self.set_tool("(p/'SCORECARD.md').write_text('rendered after the kill\\n')\n")
        self.append("after-kill")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertEqual("rendered after the kill", git(self.remote, "show", "main:scorecard/SCORECARD.md"))

    def test_a_publisher_killed_after_copy_back_is_cleaned_by_the_next_run(self):
        # devbig030, 2026-10-03: render output copied into the writer and never
        # committed made every later publication refuse "unpublished work"
        # until an operator rescued it. SIGKILL (no finally runs) between the
        # copy-back and the commit reproduces that writer; the next run saves
        # the files to a recovery ref, cleans the writer and publishes.
        self.append("before-copy-back-kill")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        self.set_tool("(p/'SCORECARD.md').write_text('copied back, never published\\n')\n"
                      "(p/'parity.json').write_text('{\"parity\":\"never published\"}\\n')\n")
        self.append("during-copy-back-kill")
        copy_back = mirror._copy_back

        def copy_back_then_die(*args, **kwargs):
            copy_back(*args, **kwargs)
            os.kill(os.getpid(), signal.SIGKILL)

        child = os.fork()
        if child == 0:
            try:
                with open(os.devnull, "w") as sink, contextlib.redirect_stderr(sink), \
                        contextlib.redirect_stdout(sink), \
                        mock.patch.object(mirror, "_copy_back", side_effect=copy_back_then_die):
                    self.publish()
            finally:
                os._exit(3)
        _, status = os.waitpid(child, 0)
        self.assertTrue(os.WIFSIGNALED(status), status)
        self.assertEqual(signal.SIGKILL, os.WTERMSIG(status))
        self.assertEqual({"scorecard/SCORECARD.md": " M", "scorecard/cells.json": " D",
                          "scorecard/cells/manifest.json": "??",
                          "scorecard/parity.json": "??"}, self._render_leftovers(writer))
        left = self._snapshot_leftovers(writer)
        killed_at = git(writer, "rev-parse", "HEAD")
        self.assertEqual(b"copied back, never published\n", left["scorecard/SCORECARD.md"])
        # Before this fix the next run refused here, and every one after it.
        self.append("after-copy-back-kill")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual(1, len(report["writer_cleaned"]), report)
        self._assert_rescued(writer, report["writer_cleaned"][0], killed_at, left)
        self.assertIn("ledger publication: WRITER CLEANED: an unpublished scorecard render left "
                      "scorecard/SCORECARD.md, scorecard/cells.json, scorecard/cells/manifest.json, "
                      "scorecard/parity.json in the ledger writer",
                      stderr.getvalue())
        self.assertEqual([row("before-copy-back-kill"), row("during-copy-back-kill"),
                          row("after-copy-back-kill")], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        absent = subprocess.run(["git", "-C", str(self.remote), "cat-file", "-e", "main:scorecard/parity.json"],
                                capture_output=True, check=False)
        self.assertNotEqual(0, absent.returncode)
        # The recovery ref stays local: nothing pushes it.
        self.assertEqual("", git(self.remote, "for-each-ref", "refs/heads/recovery"))

    def test_a_publisher_killed_part_way_through_a_write_is_cleaned_by_the_next_run(self):
        # Review of a3f62cfee1: the copy-back wrote each file straight into
        # the writer, so a run killed part way through one (here SIGXFSZ
        # after 4096 of 18000 bytes) left a prefix no render record names,
        # and every later run refused the writer. Each file is now written
        # whole outside the writer and then linked in, so such a kill leaves
        # the path absent, which the record names as possible.
        self.append("before-partial-write")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        scorecard = "scorecard/SCORECARD.md"
        stored = (writer / scorecard).read_bytes()
        child = os.fork()
        if child == 0:
            try:
                import ctypes
                import resource
                # No core dump: core_pattern may pipe one to a helper, which
                # RLIMIT_CORE alone does not stop.
                ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE 0
                resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
                resource.setrlimit(resource.RLIMIT_FSIZE, (4096, resource.RLIM_INFINITY))
                signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
                with open(os.devnull, "w") as sink, contextlib.redirect_stderr(sink):
                    mirror._replace_writer_files(writer, {scorecard: b"x" * 18000})
            finally:
                os._exit(3)
        _, status = os.waitpid(child, 0)
        self.assertTrue(os.WIFSIGNALED(status), status)
        self.assertEqual(signal.SIGXFSZ, os.WTERMSIG(status))
        self.assertFalse(os.path.lexists(writer / scorecard))
        self.assertEqual({scorecard: " D"}, mirror._worktree_changes(writer))
        # The part that was written is beside the writer, not in it.
        partial = sorted(writer.parent.glob(f"{writer.name}.staging-*/*"))
        self.assertEqual([4096], [path.stat().st_size for path in partial])
        self.append("after-partial-write")
        with contextlib.redirect_stderr(io.StringIO()):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertRegex(" ".join(report["writer_cleaned"]), "saved as refs/heads/recovery/render-")
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual(stored, (writer / scorecard).read_bytes())
        self.assertEqual([row("before-partial-write"), row("after-partial-write")], self.remote_rows())

    def test_a_publication_refused_after_copy_back_leaves_the_writer_clean(self):
        # The 18:40Z failure: the render was copied back, then the derived
        # proof refused. write_metadata, the next step of the same success
        # branch, refuses here; the run itself saves and cleans the writer.
        self.append("refused-after-copy-back")
        writer = self.parent / mirror.WRITER_RELATIVE
        refusal = mirror.MirrorRefused("derived census requires an original terminal clean schema-5 full or label run")
        left: dict[str, bytes | None] = {}

        def snapshot_then_refuse(repo, *args, **kwargs):
            left.update(self._snapshot_leftovers(repo))
            raise refusal

        stderr = io.StringIO()
        with mock.patch.object(mirror, "write_metadata", side_effect=snapshot_then_refuse), \
                contextlib.redirect_stderr(stderr):
            report = self.publish()
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual({"state": "failed", "error": str(refusal)}, report["scorecard"])
        self.assertEqual(b"derived from existing history\n", left["scorecard/SCORECARD.md"])
        self.assertEqual(1, len(report["writer_cleaned"]), report)
        self._assert_rescued(writer, report["writer_cleaned"][0], report["series_commit"], left)
        self.assertIn("WRITER CLEANED", stderr.getvalue())
        self.assertEqual([row("refused-after-copy-back")], self.remote_rows())
        self.assertEqual("legacy scorecard", git(self.remote, "show", "main:scorecard/SCORECARD.md"))
        self.append("next-run")
        report = self.publish()
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertNotIn("writer_cleaned", report)

    def test_a_start_that_finds_the_writer_clean_forgets_the_render_record(self):
        # The render record vouches only for what a refused or killed
        # copy-back left in the writer. A writer a publication found clean
        # holds nothing it could vouch for, so a later hand removal of a path
        # it recorded removing is the operator's and is refused, not cleaned.
        self.append("forget-scope")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        mirror._record_render_provenance(writer, [("scorecard/cells.json", None)])
        self.assertEqual({"scorecard/cells.json": {None}}, mirror._read_render_provenance(writer, head))
        with mirror.publication_writer(self.parent, net_wrapper="/usr/bin/env"):
            # Gone once the start checks pass, before the publication runs
            # (the end of a clean publication forgets it as well, so only
            # this check tells the two apart).
            self.assertFalse(os.path.lexists(mirror._render_provenance_path(writer)))
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        self.assertFalse(os.path.lexists(mirror._render_provenance_path(writer)))
        (writer / "scorecard/cells.json").unlink()
        changes = mirror._worktree_changes(writer)
        self.assertEqual(["scorecard/cells.json"], list(changes))
        self.assertIsNone(mirror.set_aside_render_leftovers(writer))
        self.assertEqual(changes, mirror._worktree_changes(writer))
        self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
        with self.assertRaisesRegex(mirror.MirrorRefused, "unpublished work"), \
                contextlib.redirect_stderr(io.StringIO()):
            self.publish(publish_only=True)
        self.assertEqual(changes, mirror._worktree_changes(writer))

    def test_a_publication_that_ends_with_the_writer_clean_forgets_the_render_record(self):
        # Review of a3f62cfee1: a successful publication left on record each
        # path its copy-back rewrote with the bytes HEAD already stored (no
        # commit changed it, so none dropped it from the record), and an
        # operator's later removal of SCORECARD.md was cleaned as render
        # output. A publication that ends with the writer clean now forgets
        # the record; the operator's removal is refused.
        writer = self.parent / mirror.WRITER_RELATIVE
        for name in ("clean-end-1", "clean-end-2"):
            self.append(name)
            report = self.publish()
            self.assertEqual("published", report["state"], report)
            self.assertEqual({}, mirror._worktree_changes(writer))
            self.assertFalse(os.path.lexists(mirror._render_provenance_path(writer)), name)
        (writer / "scorecard/SCORECARD.md").unlink()
        changes = mirror._worktree_changes(writer)
        self.assertEqual({"scorecard/SCORECARD.md": " D"}, changes)
        self.assertIsNone(mirror.set_aside_render_leftovers(writer))
        self.assertEqual(changes, mirror._worktree_changes(writer))
        self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
        with self.assertRaisesRegex(mirror.MirrorRefused, "unpublished work"), \
                contextlib.redirect_stderr(io.StringIO()):
            self.publish(publish_only=True)
        self.assertEqual(changes, mirror._worktree_changes(writer))

    def _self_heals(self) -> list[dict]:
        log = self.parent / mirror.SELF_HEAL_LOG_RELATIVE
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def test_a_render_of_an_unknown_commit_is_set_aside_and_named(self):
        # 2026-10-11 12:11Z: scorecard files of hermit 6c0043760f71, a commit
        # on no remote and in no run record, were written into the writer with
        # no render record, and every publication refused the writer for
        # 1 h 45 min. They are now kept in a recovery commit, the writer is
        # cleaned, and the health tick is told.
        self.append("foreign-render")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        latest = json.loads((writer / "latest.json").read_text())
        unknown = "6c0043760f71f7778dd6fa20dac11e3de7823b10"
        latest["scorecard_hermit_commit"] = unknown
        (writer / "latest.json").write_text(json.dumps(latest, sort_keys=True) + "\n")
        (writer / "scorecard/SCORECARD.md").write_text("a render nobody published\n")
        left = self._snapshot_leftovers(writer)
        self.assertEqual({"latest.json", "scorecard/SCORECARD.md"}, set(left))
        # Without the parent nothing can tell it from an operator's edit.
        self.assertIsNone(mirror.set_aside_render_leftovers(writer))
        # A commit a run record names is one a publication renders.
        runs = self.parent / "ignored" / "validate" / "runs"
        runs.mkdir(parents=True, exist_ok=True)
        (runs / "validate-known.json").write_text(json.dumps({"target": unknown}))
        self.assertIsNone(mirror.set_aside_render_leftovers(writer, self.parent))
        (runs / "validate-known.json").unlink()
        self.assertEqual([], self._self_heals())
        outcome = mirror.set_aside_render_leftovers(writer, self.parent)
        self.assertIsNotNone(outcome)
        self.assertIn(f"scorecard output of hermit {unknown}, which no publication renders", outcome)
        self.assertIn(f"writer cleaned at {head}", outcome)
        recovery = re.search(r"saved as (refs/heads/recovery/render-[0-9a-f]{40})", outcome).group(1)
        for path, content in left.items():
            self.assertEqual(content, subprocess.run(
                ["git", "-C", str(writer), "cat-file", "blob", f"{recovery}:{path}"],
                capture_output=True, check=True).stdout, path)
        self.assertEqual(mirror.FOREIGN_RENDER_RESCUE_COMMIT_MESSAGE,
                         git(writer, "log", "-1", "--format=%s", recovery))
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual(["foreign-render"], [entry["kind"] for entry in self._self_heals()])
        self.assertEqual(outcome, self._self_heals()[0]["detail"])

    def test_an_edit_that_leaves_latest_json_alone_is_not_a_foreign_render(self):
        self.append("foreign-edit")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        (writer / "scorecard/SCORECARD.md").write_text("an operator's edit\n")
        changes = mirror._worktree_changes(writer)
        self.assertIsNone(mirror.set_aside_render_leftovers(writer, self.parent))
        self.assertEqual(changes, mirror._worktree_changes(writer))
        self.assertEqual([], self._self_heals())

    def test_a_stale_writer_index_lock_is_cleared_and_named(self):
        self.append("stale-lock")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        lock = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-path", "index.lock"))
        lock.write_bytes(b"")
        # Recent: a live git command may hold it.
        self.assertIsNone(mirror.clear_stale_writer_index_lock(writer, self.parent))
        self.assertTrue(lock.exists())
        old = time.time() - mirror.STALE_INDEX_LOCK_SECONDS - 60
        os.utime(lock, (old, old))
        # Old, but a live process works in the writer.
        holder = subprocess.Popen(["sleep", "30"], cwd=writer)
        try:
            self.assertIsNone(mirror.clear_stale_writer_index_lock(writer, self.parent))
        finally:
            holder.kill()
            holder.wait()
        self.assertTrue(lock.exists())
        self.assertEqual([], self._self_heals())
        outcome = mirror.clear_stale_writer_index_lock(writer, self.parent)
        self.assertIn(f"removed a stale {lock}", outcome)
        self.assertFalse(lock.exists())
        self.assertEqual(["stale-index-lock"], [entry["kind"] for entry in self._self_heals()])
        # The publication start runs it: a later stale lock no longer stops a publication.
        lock.write_bytes(b"")
        os.utime(lock, (old, old))
        self.append("after-stale-lock")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertFalse(lock.exists())
        self.assertEqual(2, len(self._self_heals()))

    def test_a_render_record_that_is_a_fifo_is_not_waited_on(self):
        # Review of a3f62cfee1: the record was opened without O_NONBLOCK, so a
        # FIFO at its name stopped every publication's start until something
        # opened the FIFO for writing.
        self.append("fifo-record")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        record = mirror._render_provenance_path(writer)
        os.mkfifo(record)
        try:
            found: list[dict] = []
            reader = threading.Thread(target=lambda: found.append(mirror._read_render_provenance(writer, head)),
                                      daemon=True)
            reader.start()
            reader.join(10)
            stuck = reader.is_alive()
            if stuck:
                # Let the waiting open return so the thread ends.
                with contextlib.suppress(OSError):
                    os.close(os.open(record, os.O_WRONLY | os.O_NONBLOCK))
                reader.join(10)
            self.assertFalse(stuck, "reading the render record waited on a FIFO")
            self.assertEqual([{}], found)
        finally:
            os.unlink(record)

    def test_the_writer_cleanup_takes_only_unpublished_render_output(self):
        self.append("cleanup-scope")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        shard = "series/hermit/fixture/2026-09.jsonl"

        leftovers = {"scorecard/SCORECARD.md": b"leftover\n", "scorecard/parity.json": b"{}\n"}

        def render_leftovers():
            # As _replace_writer_files does: on record before it writes.
            mirror._record_render_provenance(writer, [*((name, None) for name in leftovers),
                                                      *leftovers.items()])
            for name, data in leftovers.items():
                (writer / name).write_bytes(data)

        def reset():
            git(writer, "update-index", "--no-assume-unchanged", shard)
            if git(writer, "ls-files", "--", "scorecard/SCORECARD.md"):
                git(writer, "update-index", "--no-assume-unchanged", "scorecard/SCORECARD.md")
            git(writer, "reset", "-q", "--hard", head)
            git(writer, "clean", "-qfdx", "--", "scorecard", "series", "stray.txt")
            mirror._forget_render_provenance(writer)
            self.assertEqual({}, mirror._worktree_changes(writer))

        def unrelated_file():
            (writer / "stray.txt").write_text("not render output\n")

        def series_row():
            with (writer / shard).open("a") as stream:
                stream.write(json.dumps(row("unspooled")) + "\n")

        def staged_then_edited():
            git(writer, "add", "scorecard/SCORECARD.md")
            (writer / "scorecard/SCORECARD.md").write_text("edited after staging\n")

        def renamed():
            git(writer, "mv", "-f", "scorecard/SCORECARD.md", "scorecard/parity.json")

        def symlink():
            (writer / "scorecard/parity.json").unlink()
            (writer / "scorecard/parity.json").symlink_to(writer / shard)

        def executable():
            git(writer, "add", "scorecard/parity.json")
            (writer / "scorecard/parity.json").chmod(0o755)

        def executable_published():
            # Review of 6f4e0db6e1: the copy-back writes no executable file, so
            # a mode change on a stored file is not its output, even when its
            # bytes are.
            (writer / "scorecard/SCORECARD.md").chmod(0o755)

        def executable_new():
            (writer / "scorecard/parity.json").chmod(0o755)

        def unpublished_head():
            git(writer, "add", "scorecard")
            git(writer, "commit", "-qm", "local, never pushed")
            (writer / "scorecard/SCORECARD.md").write_text("after a local commit\n")

        def unregistered_part():
            (writer / "scorecard/cells").mkdir()
            (writer / "scorecard/cells/part-x").write_text("not a part name\n")

        def hidden_edit():
            # Status cannot see this row; a reset to HEAD would destroy it.
            git(writer, "update-index", "--assume-unchanged", shard)
            series_row()

        def index_bit():
            # Status shows the staged bytes; the edit after them is hidden.
            git(writer, "add", "scorecard/SCORECARD.md")
            git(writer, "update-index", "--assume-unchanged", "scorecard/SCORECARD.md")
            (writer / "scorecard/SCORECARD.md").write_text("hidden after staging\n")

        def removed_and_new():
            # "D " and "??" under one name: an unlink after the reset would
            # delete the copy HEAD stores.
            git(writer, "rm", "-q", "--cached", "scorecard/SCORECARD.md")

        cases = {"an unrelated file": unrelated_file, "a series row": series_row,
                 "a staged path edited again": staged_then_edited, "a rename": renamed,
                 "a symbolic link": symlink, "an executable bit": executable,
                 "an executable bit on a published file": executable_published,
                 "an executable new file": executable_new,
                 "an unpublished HEAD": unpublished_head, "an unknown cells name": unregistered_part,
                 "an edit the status view hides": hidden_edit, "an index bit": index_bit,
                 "a removal and a new file under one name": removed_and_new}
        record = mirror._render_provenance_path(writer)

        def operator_edit():
            (writer / "scorecard/SCORECARD.md").write_text("operator edit\n")

        def other_bytes():
            # What the render recorded, then other bytes over one of them.
            render_leftovers()
            operator_edit()

        def record_at_another_head():
            render_leftovers()
            value = json.loads(record.read_text())
            self.assertEqual([head], value["heads"])
            value["heads"] = ["0" * 40]
            record.write_text(json.dumps(value))

        def earlier_record_at_another_head():
            # As a3f62cfee1 wrote it: one "head".
            render_leftovers()
            value = json.loads(record.read_text())
            del value["heads"]
            value["head"] = "0" * 40
            record.write_text(json.dumps(value))

        def linked_record():
            render_leftovers()
            outside = self.root / "outside-provenance.json"
            outside.write_bytes(record.read_bytes())
            record.unlink()
            record.symlink_to(outside)

        def malformed_record():
            render_leftovers()
            value = json.loads(record.read_text())
            value["paths"]["scorecard/SCORECARD.md"].append("not a digest")
            record.write_text(json.dumps(value))

        def oversized_record():
            # A valid record followed by whitespace: its first
            # _RENDER_PROVENANCE_LIMIT bytes would parse on their own.
            render_leftovers()
            record.write_text(record.read_text() + " " * mirror._RENDER_PROVENANCE_LIMIT)

        def operator_removal():
            # No render recorded removing it.
            render_leftovers()
            (writer / "scorecard/cells.json").unlink()

        # Review of 0287e858e4: a change of the shape a render leaves is not
        # proof that one made it. Changes no publication recorded writing at
        # this HEAD, with these bytes, are the operator's and are refused,
        # with the record missing, naming other bytes, made at another HEAD,
        # reached through a link, malformed or too large.
        operator_cases = {"an operator edit with nothing on record": operator_edit,
                          "bytes other than the ones on record": other_bytes,
                          "a record made at another HEAD": record_at_another_head,
                          "a record in the earlier form made at another HEAD": earlier_record_at_another_head,
                          "a record that is a symbolic link": linked_record,
                          "a malformed record": malformed_record,
                          "a record over its size limit": oversized_record,
                          "a removal not on record": operator_removal}
        self.assertEqual(["scorecard/cells.json"], git(writer, "ls-files", "--", "scorecard/cells.json").splitlines())
        for label, damage in [*((label, (lambda damage=damage: (render_leftovers(), damage())))
                                for label, damage in cases.items()), *operator_cases.items()]:
            with self.subTest(label):
                damage()
                before = git(writer, "status", "--porcelain", "--untracked-files=all")
                writer_head = git(writer, "rev-parse", "HEAD")
                shard_bytes = (writer / shard).read_bytes()
                self.assertIsNone(mirror.set_aside_render_leftovers(writer))
                self.assertEqual(before, git(writer, "status", "--porcelain", "--untracked-files=all"))
                self.assertEqual(writer_head, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(shard_bytes, (writer / shard).read_bytes())
                self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
                with self.assertRaisesRegex(mirror.MirrorRefused, "unpublished work"), \
                        contextlib.redirect_stderr(io.StringIO()):
                    self.publish(publish_only=True)
                self.assertEqual(before, git(writer, "status", "--porcelain", "--untracked-files=all"))
                self.assertEqual(writer_head, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(shard_bytes, (writer / shard).read_bytes())
                reset()
        # Bytes that change after the record check (here, as the unstored-
        # bytes census runs) are held to the record again once saved, so
        # the operator's are refused, not saved and reset.
        with self.subTest("bytes changed after the record was checked"):
            render_leftovers()
            census = mirror._unstored_paths

            def changed_after_check(path):
                (writer / "scorecard/SCORECARD.md").write_bytes(b"operator edit after the check\n")
                return census(path)

            before = git(writer, "rev-parse", "HEAD")
            with mock.patch.object(mirror, "_unstored_paths", side_effect=changed_after_check):
                self.assertIsNone(mirror.set_aside_render_leftovers(writer))
            self.assertEqual(b"operator edit after the check\n", (writer / "scorecard/SCORECARD.md").read_bytes())
            self.assertEqual(leftovers["scorecard/parity.json"], (writer / "scorecard/parity.json").read_bytes())
            self.assertEqual(before, git(writer, "rev-parse", "HEAD"))
            self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
            reset()
        # The same leftovers alone are cleaned, with the record in the form
        # a3f62cfee1 wrote too.
        render_leftovers()
        value = json.loads(record.read_text())
        record.write_text(json.dumps({"head": value["heads"][0], "paths": value["paths"]}))
        left = self._snapshot_leftovers(writer)
        with contextlib.redirect_stderr(io.StringIO()):
            outcome = mirror.set_aside_render_leftovers(writer)
        self.assertIsNotNone(outcome)
        found = re.search(r"saved as (refs/heads/recovery/render-[0-9a-f]{40})", outcome)
        commit = git(writer, "rev-parse", found.group(1))
        self.assertEqual(sorted(left), git(writer, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                           head, commit).splitlines())
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        # A publication killed after its `git add` and before its commit
        # leaves the same leftovers staged. They are cleaned too, and the
        # index goes back to HEAD with the files.
        with self.subTest("the same leftovers staged by the publication's add"):
            stored = {path: (writer / path).read_bytes()
                      for path in ("scorecard/SCORECARD.md", "scorecard/cells.json")}
            render_leftovers()
            mirror._record_render_provenance(writer, [("scorecard/cells.json", None)])
            (writer / "scorecard/cells.json").unlink()
            git(writer, "add", "--all", "--", "scorecard")
            self.assertEqual({"scorecard/SCORECARD.md": "M ", "scorecard/cells.json": "D ",
                              "scorecard/parity.json": "A "}, mirror._worktree_changes(writer))
            with contextlib.redirect_stderr(io.StringIO()):
                outcome = mirror.set_aside_render_leftovers(writer)
            self.assertIsNotNone(outcome)
            commit = git(writer, "rev-parse", re.search(r"saved as (refs/heads/recovery/render-[0-9a-f]{40})",
                                                        outcome).group(1))
            self.assertEqual(["scorecard/SCORECARD.md", "scorecard/cells.json", "scorecard/parity.json"],
                             git(writer, "diff-tree", "--no-commit-id", "--name-only", "-r",
                                 head, commit).splitlines())
            self.assertEqual({}, mirror._worktree_changes(writer))
            self.assertEqual("", git(writer, "diff", "--cached", "--name-only", head))
            self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
            self.assertEqual(stored, {path: (writer / path).read_bytes() for path in stored})
            self.assertFalse(os.path.lexists(writer / "scorecard/parity.json"))
        # Review of 36da12785d: the copy-back writes each file anew, not
        # executable, so over a file HEAD stores as executable it leaves
        # 100755 -> 100644, and it may remove such a file. Both are its
        # output and are cleaned; a new executable bit (above) still is not.
        executable_paths = ["scorecard/SCORECARD.md", "scorecard/cells.json"]
        self.assertEqual(executable_paths, git(writer, "ls-files", "--", *executable_paths).splitlines())
        git(writer, "update-index", "--chmod=+x", "--", *executable_paths)
        for path in executable_paths:
            (writer / path).chmod(0o755)
        git(writer, "commit", "-qm", "fixture: published executable scorecard files")
        git(writer, "push", "-q", "origin", "HEAD:refs/heads/main")
        git(writer, "fetch", "-q", "origin", "main")
        executable_head = git(writer, "rev-parse", "HEAD")
        self.assertEqual(executable_head, git(self.remote, "rev-parse", "main"))
        # Review of 0287e858e4: with no render on record, the same shapes
        # made by hand (the executable bit cleared, with or without an edit,
        # or the file removed) are the operator's, and are refused.
        published = (writer / "scorecard/SCORECARD.md").read_bytes()

        def cleared(edit: bytes | None = None) -> None:
            (writer / "scorecard/SCORECARD.md").chmod(0o644)
            if edit is not None:
                (writer / "scorecard/SCORECARD.md").write_bytes(edit)

        for label, damage in (("the executable bit cleared by hand", cleared),
                              ("the executable bit cleared and the file edited by hand",
                               lambda: cleared(b"operator edit\n")),
                              ("an executable file removed by hand",
                               lambda: (writer / "scorecard/SCORECARD.md").unlink())):
            with self.subTest(label):
                mirror._forget_render_provenance(writer)
                refs = git(writer, "for-each-ref", "refs/heads/recovery/")
                damage()
                before = git(writer, "status", "--porcelain", "--untracked-files=all")
                self.assertNotEqual("", before)
                self.assertIsNone(mirror.set_aside_render_leftovers(writer))
                self.assertEqual(before, git(writer, "status", "--porcelain", "--untracked-files=all"))
                self.assertEqual(executable_head, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(refs, git(writer, "for-each-ref", "refs/heads/recovery/"))
                git(writer, "reset", "-q", "--hard", executable_head)
                self.assertEqual(published, (writer / "scorecard/SCORECARD.md").read_bytes())
                self.assertEqual({}, mirror._worktree_changes(writer))
        (writer / "scorecard/SCORECARD.md").unlink()
        render_leftovers()
        mirror._record_render_provenance(writer, [("scorecard/cells.json", None)])
        (writer / "scorecard/cells.json").unlink()
        left = self._snapshot_leftovers(writer)
        with contextlib.redirect_stderr(io.StringIO()):
            outcome = mirror.set_aside_render_leftovers(writer)
        self.assertIsNotNone(outcome)
        commit = git(writer, "rev-parse", re.search(r"saved as (refs/heads/recovery/render-[0-9a-f]{40})",
                                                    outcome).group(1))
        raw = git(writer, "diff-tree", "--no-commit-id", "-r", executable_head, commit).splitlines()
        self.assertEqual([(":100755", "100644", "M", "scorecard/SCORECARD.md"),
                          (":100755", "000000", "D", "scorecard/cells.json"),
                          (":000000", "100644", "A", "scorecard/parity.json")],
                         [(line.split()[0], line.split()[1], line.split()[4], line.split()[5]) for line in raw])
        self.assertEqual(sorted(left), sorted(line.split()[5] for line in raw))
        self.assertEqual({}, mirror._worktree_changes(writer))
        self.assertEqual(executable_head, git(writer, "rev-parse", "HEAD"))

    def test_the_writer_cleanup_keeps_bytes_written_after_its_check(self):
        # Review of a3f62cfee1: the cleanup compared each leftover with its
        # saved copy and then ran `git reset --hard`, which rewrote every
        # path from HEAD and erased bytes another process wrote to one in
        # between. Each file is now renamed aside and compared there, and
        # HEAD's bytes go only where nothing has appeared. The writes here
        # come just after the recovery ref is made, the last step before the
        # paths change.
        self.append("cleanup-race")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        operator = b"operator bytes written during the cleanup\n"
        leftovers = {"scorecard/SCORECARD.md": b"leftover\n", "scorecard/parity.json": b"{}\n"}
        real_git = mirror._git

        def displaced() -> list[Path]:
            return sorted(writer.parent.glob(f"{writer.name}.displaced-*"))

        def reset():
            git(writer, "reset", "-q", "--hard", head)
            git(writer, "clean", "-qfdx", "--", "scorecard")
            mirror._forget_render_provenance(writer)
            self.assertEqual({}, mirror._worktree_changes(writer))

        def leave_leftovers(removed: tuple[str, ...] = ()):
            # As _replace_writer_files does: on record before it writes.
            mirror._record_render_provenance(writer, [*((name, None) for name in (*leftovers, *removed)),
                                                      *leftovers.items()])
            for name, data in leftovers.items():
                (writer / name).write_bytes(data)
            for name in removed:
                (writer / name).unlink()

        def writing_after_the_ref(path: str):
            def wrapped(repo, *args, **kwargs):
                result = real_git(repo, *args, **kwargs)
                if "update-ref" in args and any(str(arg).startswith("refs/heads/recovery/render-")
                                                for arg in args):
                    (writer / path).write_bytes(operator)
                return result
            return wrapped

        def clean() -> str | None:
            with contextlib.redirect_stderr(io.StringIO()):
                return mirror.set_aside_render_leftovers(writer)

        with self.subTest("leftover rewritten after it was compared"):
            leave_leftovers()
            before = displaced()
            with mock.patch.object(mirror, "_git", side_effect=writing_after_the_ref("scorecard/SCORECARD.md")), \
                    self.assertRaisesRegex(mirror.MirrorRefused,
                                           "render leftover scorecard/SCORECARD.md changed after it was saved in "
                                           "[0-9a-f]{40}: it was moved to .* and kept, and the cleanup stopped"):
                clean()
            [retained] = [path for path in displaced() if path not in before]
            self.assertEqual(operator, (retained / "files/scorecard/SCORECARD.md").read_bytes())
            # Not listed, so no publication empties it.
            self.assertFalse(os.path.lexists(retained / "displaced.json"))
            self.assertEqual({"scorecard/SCORECARD.md": " D", "scorecard/parity.json": "??"},
                             mirror._worktree_changes(writer))
            # What is left is still on record, and the next cleanup takes it.
            self.assertRegex(clean() or "", "saved as refs/heads/recovery/render-")
            self.assertEqual({}, mirror._worktree_changes(writer))
            self.assertEqual(operator, (retained / "files/scorecard/SCORECARD.md").read_bytes())
            self.assertFalse(os.path.lexists(retained / "displaced.json"))
        with self.subTest("removed leftover written again after it was compared"):
            reset()
            leave_leftovers(removed=("scorecard/cells.json",))
            with mock.patch.object(mirror, "_git", side_effect=writing_after_the_ref("scorecard/cells.json")), \
                    self.assertRaisesRegex(mirror.MirrorRefused,
                                           "ledger writer path scorecard/cells.json was written by another "
                                           "process while the cleanup returned it to HEAD; it was left there"):
                clean()
            self.assertEqual(operator, (writer / "scorecard/cells.json").read_bytes())
            self.assertEqual({"scorecard/cells.json": " M", "scorecard/parity.json": "??"},
                             mirror._worktree_changes(writer))
            # Not render output: the next cleanup leaves it for the refusal.
            self.assertIsNone(clean())
            self.assertEqual(operator, (writer / "scorecard/cells.json").read_bytes())
        with self.subTest("cleanup killed after a file was moved and before HEAD's bytes went back"):
            reset()
            # A scorecard file that changed while the renderer ran is on
            # record with its bytes only, not as possibly absent.
            window = b"written while the renderer ran\n"
            mirror._record_render_provenance(writer, [("scorecard/SCORECARD.md", window)])
            (writer / "scorecard/SCORECARD.md").write_bytes(window)

            class Killed(BaseException):
                pass

            with mock.patch.object(mirror, "_install_exclusive", side_effect=Killed), self.assertRaises(Killed):
                clean()
            self.assertEqual({"scorecard/SCORECARD.md": " D"}, mirror._worktree_changes(writer))
            self.assertRegex(clean() or "", "saved as refs/heads/recovery/render-")
            self.assertEqual({}, mirror._worktree_changes(writer))
            self.assertEqual("legacy scorecard\n", (writer / "scorecard/SCORECARD.md").read_text())

    def test_a_publication_still_refuses_work_it_cannot_prove_derived(self):
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        prefix = mirror.finalized_proof.PREFIX
        proof = self._proof_files("refused-run")
        carrier = next(path for path in proof if "/runs/" in path)
        operands = next(path for path in proof if "/objects/" in path)
        unreferenced = b"an object no carrier names\n"
        shard = "series/hermit/fixture/2026-09.jsonl"
        smuggled = json.dumps(row("smuggled")) + "\n"
        grafts = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-path", "info/grafts"))
        stranded_scorecard = {"scorecard/SCORECARD.md": "stranded\n"}
        metadata = self._scorecard_metadata
        parts = mirror.scorecard_cells.split(b'{"legacy":"preserve"}\n')

        def replaced_row_commit() -> str:
            # https://github.com/rrnewton/dev-hermit/issues/447 review: a
            # replacement ref makes a row-carrying commit read as a qualifying
            # derived one, and checking it out under the replacement leaves a
            # clean worktree.
            derived = self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE)
            git(writer, "checkout", "-q", "--detach", base)
            rows = self._strand(writer, {shard: json.dumps(row("smuggled")) + "\n"},
                                "fixture: unspooled row commit")
            git(writer, "checkout", "-q", "--detach", base)
            git(writer, "replace", rows, derived)
            git(writer, "checkout", "-q", "--detach", rows)
            return rows

        def strand_under_graft() -> str:
            head = self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE)
            grafts.write_text("")
            return head

        def replaced_under_a_custom_base() -> str:
            # Review of 9c64201316: Git reads replacements under any
            # GIT_REPLACE_REF_BASE (here refs/custom-<sha>). The stored HEAD
            # qualifies while the checkout shows an unspooled row commit, so
            # freeing it would have removed that row from the work tree.
            derived = self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE)
            git(writer, "checkout", "-q", "--detach", base)
            rows = self._strand(writer, {shard: smuggled}, "fixture: unspooled row commit")
            git(writer, "checkout", "-q", "--detach", base)
            os.environ["GIT_REPLACE_REF_BASE"] = "refs/custom-"
            git(writer, "replace", derived, rows)
            git(writer, "checkout", "-q", "--detach", derived)
            return derived

        def on_a_published_ignored_submodule() -> None:
            # Main declares submodule "vendor" with ignore = all, which Git's
            # diff and status honour. Its empty directory is an uninitialized
            # submodule, so a gitlink there differs from nothing on disk.
            git(writer, "fetch", "-q", "origin", "main")
            git(writer, "checkout", "-q", "--detach", "refs/remotes/origin/main")
            if not (writer / ".gitmodules").exists():
                (writer / ".gitmodules").write_text(
                    '[submodule "vendor"]\n\tpath = vendor\n\turl = ./vendor\n\tignore = all\n')
                git(writer, "add", ".gitmodules")
                git(writer, "commit", "-qm", "fixture: published submodule declaration")
                git(writer, "push", "-q", "origin", "HEAD:refs/heads/main")
            (writer / "vendor").mkdir(exist_ok=True)

        def gitlink_hidden_by_its_ignore_setting() -> str:
            # Review of 9c64201316: diff-tree honours submodule.<name>.ignore,
            # so a gitlink added beside a valid proof was not listed.
            on_a_published_ignored_submodule()
            git(writer, "update-index", "--add", "--cacheinfo", f"160000,{base},vendor")
            return self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE)

        cases = (
            ("foreign message", lambda: self._strand(writer, proof, "fixture: operator work")),
            ("derived message over a path no derived commit writes",
             lambda: self._strand(writer, {"notes.txt": "operator notes\n"},
                                  mirror.PROOF_FALLBACK_COMMIT_MESSAGE)),
            ("operator file under the proof directory",
             lambda: self._strand(writer, {f"{prefix}/operator-notes.txt": "notes\n"},
                                  mirror.PROOF_COMMIT_MESSAGE)),
            ("proof message changing a scorecard file",
             lambda: self._strand(writer, {**proof, "scorecard/SCORECARD.md": "changed\n"},
                                  mirror.PROOF_COMMIT_MESSAGE)),
            ("derived message with a further body line",
             lambda: self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE + "\n\nand something else")),
            ("derived message stored with a CRLF ending",
             lambda: self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE + "\r",
                                  commit_args=("--cleanup=verbatim",))),
            ("scorecard message carrying a series shard latest.json names",
             lambda: self._strand(writer, {"latest.json": metadata({"scorecard/SCORECARD.md":
                                                                    "legacy scorecard\n",
                                                                    shard: smuggled}),
                                           shard: smuggled}, mirror.SCORECARD_COMMIT_MESSAGE)),
            ("scorecard message making a scorecard file executable",
             lambda: self._strand(writer, {"latest.json": metadata({"scorecard/SCORECARD.md":
                                                                    "legacy scorecard\n"})},
                                  mirror.SCORECARD_COMMIT_MESSAGE, executable=("scorecard/SCORECARD.md",))),
            ("scorecard message storing the history with an executable part",
             lambda: self._strand(writer, {"latest.json": metadata(stranded_scorecard), **stranded_scorecard,
                                           **parts}, mirror.SCORECARD_COMMIT_MESSAGE,
                                  executable=(mirror.scorecard_cells.part_path(0),),
                                  removed=(mirror.scorecard_cells.LOGICAL_PATH,))),
            ("scorecard message adding a part beside the single-file history",
             lambda: self._strand(writer, {"latest.json": metadata(stranded_scorecard), **stranded_scorecard,
                                           "scorecard/cells/part-9999": "unlisted part\n"},
                                  mirror.SCORECARD_COMMIT_MESSAGE)),
            ("scorecard file whose bytes differ from latest.json",
             lambda: self._strand(writer, {"latest.json": metadata({"scorecard/SCORECARD.md": "named\n"}),
                                           **stranded_scorecard}, mirror.SCORECARD_COMMIT_MESSAGE)),
            ("scorecard change latest.json does not name",
             lambda: self._strand(writer, {"latest.json": metadata(stranded_scorecard), **stranded_scorecard,
                                           "scorecard/parity.json": "{}\n"}, mirror.SCORECARD_COMMIT_MESSAGE)),
            ("scorecard message without latest.json",
             lambda: self._strand(writer, stranded_scorecard, mirror.SCORECARD_COMMIT_MESSAGE)),
            ("latest.json that is not scorecard metadata",
             lambda: self._strand(writer, {"latest.json": "{}\n", **stranded_scorecard},
                                  mirror.SCORECARD_COMMIT_MESSAGE)),
            ("scorecard message adding latest.json as a symlink",
             lambda: self._strand(writer, {}, mirror.SCORECARD_COMMIT_MESSAGE,
                                  symlinks={"latest.json": "scorecard/SCORECARD.md"})),
            ("carrier stored as a symlink",
             lambda: self._strand(writer, {operands: proof[operands]}, mirror.PROOF_COMMIT_MESSAGE,
                                  symlinks={carrier: "../objects/sha256/" + operands.rsplit("/", 1)[1]})),
            ("object stored as a symlink to its own bytes",
             lambda: self._strand(writer, {carrier: proof[carrier]}, mirror.PROOF_COMMIT_MESSAGE,
                                  symlinks={operands: proof[operands].decode()})),
            ("carrier that is not a valid manifest",
             lambda: self._strand(writer, {carrier: "{}\n", operands: proof[operands]},
                                  mirror.PROOF_COMMIT_MESSAGE)),
            ("valid carrier under another carrier name",
             lambda: self._strand(writer, {f"{prefix}/runs/{'c' * 64}.json": proof[carrier],
                                           operands: proof[operands]}, mirror.PROOF_COMMIT_MESSAGE)),
            ("object whose bytes differ from its name",
             lambda: self._strand(writer, {carrier: proof[carrier], operands: b"other bytes\n"},
                                  mirror.PROOF_COMMIT_MESSAGE)),
            ("object no added carrier references",
             lambda: self._strand(writer, {**proof, f"{prefix}/objects/sha256/"
                                           + hashlib.sha256(unreferenced).hexdigest(): unreferenced},
                                  mirror.PROOF_COMMIT_MESSAGE)),
            ("carrier nested past the recursion limit",
             lambda: self._strand(writer, {f"{prefix}/runs/{'d' * 64}.json": "[" * 100_000 + "]" * 100_000 + "\n"},
                                  mirror.PROOF_COMMIT_MESSAGE)),
            ("row-carrying commit replaced by a qualifying derived commit", replaced_row_commit),
            ("qualifying derived commit replaced under a custom replacement base",
             replaced_under_a_custom_base),
            ("qualifying derived commit while a graft file exists", strand_under_graft),
            ("gitlink hidden by its submodule ignore setting", gitlink_hidden_by_its_ignore_setting),
            ("two derived commits, the lower one also unpublished",
             lambda: (self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE),
                      self._strand(writer, self._proof_files("stacked-run"), mirror.PROOF_COMMIT_MESSAGE))[1]),
        )
        # A writer under a replacement ref or graft file is refused when the
        # publication starts, before any ancestry decision, by a message that
        # names the overlay (_refuse_history_overlay).
        overlay_cases = {"row-carrying commit replaced by a qualifying derived commit",
                         "qualifying derived commit replaced under a custom replacement base",
                         "qualifying derived commit while a graft file exists"}
        # Each case starts from the published base with no recovery ref,
        # replacement ref or graft file, so a case the classifier wrongly
        # accepts fails alone, not every later one.
        for name, strand in cases:
            with self.subTest(name):
                try:
                    head = strand()
                    expected = (f"ledger writer at {head} is under a .+, which makes Git show other "
                                "history; nothing was published and the writer is retained for recovery"
                                if name in overlay_cases else
                                f"ledger writer has unpublished derived or unknown work at {head}; "
                                "retained for recovery")
                    with self.assertRaisesRegex(mirror.MirrorRefused, expected):
                        self.publish(publish_only=True)
                    self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
                    self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
                    self.assertEqual([row("published-first")], self.remote_rows())
                finally:
                    # Back to the base first, while a replacement still shows
                    # the tree that was checked out, then drop the overlays.
                    git(writer, "checkout", "-q", "-f", "--detach", base)
                    for ref in git(writer, "for-each-ref", "--format=%(refname)").split():
                        if ref.startswith(("refs/heads/recovery/", "refs/replace/", "refs/custom-")):
                            git(writer, "update-ref", "-d", ref)
                    os.environ.pop("GIT_REPLACE_REF_BASE", None)
                    if grafts.exists():
                        grafts.unlink()
                    self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
        # A derived commit whose recovery name already holds other work stays.
        head = self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE)
        git(writer, "update-ref", f"refs/heads/recovery/derived-{head}", base)
        with self.assertRaisesRegex(
                mirror.MirrorRefused,
                f"ledger writer is stranded on derived commit {head} and could not be freed: writer "
                f"retained at {head}: recovery ref refs/heads/recovery/derived-{head} already names "
                "different work"):
            self.publish(publish_only=True)
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        git(writer, "update-ref", "-d", f"refs/heads/recovery/derived-{head}")
        git(writer, "checkout", "-q", "--detach", base)
        self.append("pending")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())

    def _publish_elsewhere(self, label: str) -> str:
        """Another host publishes one commit to main; returns it."""
        other = self.root / f"other-host-{label}"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        git(other, "commit", "--allow-empty", "-qm", f"another host published ({label})")
        git(other, "push", "-q", "origin", "main")
        return git(other, "rev-parse", "HEAD")

    def test_a_writer_under_a_graft_is_refused_before_any_ancestry_decision(self):
        # Review of 33e15225cb: a graft gives the next published commit an
        # unpublished row commit as its parent, so ancestry reads that commit
        # as published. Startup and recover_spooled_commit returned None, and
        # the checkout to main then dropped its unspooled row with no ref.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        shard = "series/hermit/fixture/2026-09.jsonl"
        smuggled = json.dumps(row("smuggled")) + "\n"
        grafts = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-path", "info/grafts"))
        unspooled = self._strand(writer, {shard: smuggled}, "fixture: unspooled row commit")
        published = self._publish_elsewhere("graft")
        git(writer, "fetch", "-q", "origin", "main")
        grafts.write_text(f"{published} {unspooled}\n")
        # The fixture makes the hazard: Git now orders the row commit under main.
        self.assertEqual(0, subprocess.run(["git", "-C", str(writer), "merge-base", "--is-ancestor",
                                            unspooled, "refs/remotes/origin/main"]).returncode)
        refusal = (f"ledger writer at {unspooled} is under a graft file '{grafts}', which makes Git show "
                   "other history; nothing was published and the writer is retained for recovery")
        # A raw publication (publish_series and recover_spooled_commit) and a
        # legacy export (which checks ancestry and checks out main itself). A
        # scorecard command publishes only a measured commit, so a run record
        # names the target.
        runs = self.parent / "ignored" / "validate" / "runs"
        runs.mkdir(parents=True, exist_ok=True)
        (runs / "validate-graft.json").write_text(json.dumps({"target": self.target}))
        for label, kwargs in (("raw publication", {"publish_only": True}),
                              ("legacy export", {"scorecard_command": "export-legacy"})):
            with self.subTest(label):
                self.append(f"pending-{label.replace(' ', '-')}")
                with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(refusal)):
                    self.publish(**kwargs)
                self.assertEqual(unspooled, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(smuggled, (writer / shard).read_text())
                self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
                self.assertEqual([row("published-first")], self.remote_rows())
        # recover_spooled_commit refuses before its own ancestry check.
        with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(refusal)):
            mirror.recover_spooled_commit(writer, self.parent)
        grafts.unlink()

    def test_a_graft_file_override_is_refused_even_when_its_name_ends_in_a_space(self):
        # Review of 33e15225cb: the graft path was read with surrounding
        # whitespace stripped, so GIT_GRAFT_FILE naming "<file> " was looked
        # up as "<file>" and missed. Under it, a valid proof on an unpublished
        # row commit qualified and was set aside onto that row commit, which
        # the graft also showed as published.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        shard = "series/hermit/fixture/2026-09.jsonl"
        smuggled = json.dumps(row("smuggled")) + "\n"
        unspooled = self._strand(writer, {shard: smuggled}, "fixture: unspooled row commit")
        head = self._strand(writer, self._proof_files("grafted-run"), mirror.PROOF_COMMIT_MESSAGE)
        published = self._publish_elsewhere("graft-override")
        override = self.root / "ledger grafts "
        override.write_text(f"{published} {unspooled}\n")
        self.addCleanup(os.environ.pop, "GIT_GRAFT_FILE", None)
        os.environ["GIT_GRAFT_FILE"] = str(override)
        self.append("pending")
        with self.assertRaisesRegex(mirror.MirrorRefused,
                                    re.escape(f"ledger writer at {head} is under a graft file override "
                                              f"GIT_GRAFT_FILE={str(override)!r}")):
            self.publish(publish_only=True)
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
        self.assertEqual([row("published-first")], self.remote_rows())

    def test_a_ref_name_that_is_not_utf8_does_not_stop_the_set_aside(self):
        # Review of 33e15225cb: ref names were decoded as UTF-8 text, so an
        # unrelated operator branch named with a 0xff byte crashed the start
        # of every publication and recover_spooled_commit.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        operator = b"refs/heads/operator-\xff"
        subprocess.run([b"git", b"-C", bytes(writer), b"update-ref", operator, base.encode()], check=True)
        self.addCleanup(subprocess.run, [b"git", b"-C", bytes(writer), b"update-ref", b"-d", operator],
                        check=True)
        stranded = self._strand(writer, self._proof_files("operator-ref-run"), mirror.PROOF_COMMIT_MESSAGE)
        self.append("pending")
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertIn(f"WRITER FREED: a previous run left the writer on derived commit {stranded}",
                      stderr.getvalue())
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())
        self.assertIsNone(mirror.recover_spooled_commit(writer, self.parent))

    def test_a_set_aside_never_overwrites_an_ignored_operator_file(self):
        # Review of 33e15225cb: a valid scorecard commit moves the history
        # from scorecard/cells.json into parts. An operator file recreated
        # there and ignored passes both cleanliness checks, and the switch
        # back to the parent replaced it with the parent's history; it is in
        # no commit and no recovery ref, so its bytes were gone.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        scorecard = {"scorecard/SCORECARD.md": "stranded\n"}
        history = mirror.scorecard_cells.LOGICAL_PATH
        head = self._strand(writer, {"latest.json": self._scorecard_metadata(scorecard), **scorecard,
                                     **mirror.scorecard_cells.split(b'{"legacy":"preserve"}\n')},
                            mirror.SCORECARD_COMMIT_MESSAGE, removed=(history,))
        exclude = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-path", "info/exclude"))
        excluded = exclude.read_bytes() if exclude.exists() else None
        exclude.parent.mkdir(parents=True, exist_ok=True)
        exclude.write_bytes((excluded or b"") + f"/{history}\n".encode())
        operator = b'{"operator":"work in progress"}\n'
        (writer / history).write_bytes(operator)
        self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
        self.append("pending")
        # Review of 16b59997d8: the publication refuses when it starts, since
        # later checkouts and the regeneration could also replace the file.
        with self.assertRaisesRegex(
                mirror.MirrorRefused,
                re.escape("ledger writer holds bytes its HEAD does not store, which a checkout or the "
                          "scorecard regeneration could overwrite with no ref left: "
                          f"{history!r} is untracked or ignored; nothing was published")):
            self.publish(publish_only=True)
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        self.assertEqual(operator, (writer / history).read_bytes())
        self.assertEqual([row("published-first")], self.remote_rows())
        # The set-aside itself still never replaces the file.
        with self.assertRaisesRegex(
                mirror.MirrorRefused,
                f"ledger writer is stranded on derived commit {head} and could not be freed: "
                f"writer retained at {head}: git failed: "):
            mirror.set_aside_stranded_derived_commit(writer)
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        self.assertEqual(operator, (writer / history).read_bytes())
        # Only the ignored file held it: without it the same commit is freed.
        (writer / history).unlink()
        if excluded is None:
            exclude.unlink()
        else:
            exclude.write_bytes(excluded)
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertIn(f"WRITER FREED: a previous run left the writer on derived commit {head}",
                      stderr.getvalue())
        self.assertEqual(head, git(writer, "rev-parse", f"refs/heads/recovery/derived-{head}"))
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())

    def test_a_scorecard_commit_reads_its_proof_files_with_the_proof_capacity(self):
        # Review of 33e15225cb: a scorecard commit's proof files were held to
        # the 100 MiB file limit, though the producer and _proof_bound allow
        # finalized_proof.ARTIFACT_BYTES_PER_ROW (256 MiB) per artifact, so a
        # combined commit was refused where the proof alone qualified. The
        # file limit is lowered here to put the proof files past it.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        limit = 64
        proof = self._proof_files("capacity-run")
        self.assertTrue(all(len(content) > limit for content in proof.values()))
        scorecard = {"scorecard/SCORECARD.md": "stranded\n", **proof}
        stranded = self._strand(writer, {"latest.json": self._scorecard_metadata(scorecard), **scorecard},
                                mirror.SCORECARD_COMMIT_MESSAGE)
        self.append("pending")
        with mock.patch.object(mirror, "_FILE_LIMIT", limit), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertEqual(stranded, git(writer, "rev-parse", f"refs/heads/recovery/derived-{stranded}"))
        # A scorecard file past the file limit is still refused.
        git(writer, "checkout", "-q", "--detach", base)
        oversized = {"scorecard/SCORECARD.md": "x" * limit + "\n"}
        head = self._strand(writer, {"latest.json": self._scorecard_metadata(oversized), **oversized},
                            mirror.SCORECARD_COMMIT_MESSAGE)
        with mock.patch.object(mirror, "_FILE_LIMIT", limit), self.assertRaisesRegex(
                mirror.MirrorRefused,
                f"ledger writer has unpublished derived or unknown work at {head}; retained for recovery"):
            self.publish(publish_only=True)
        self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())

    def test_a_proof_is_held_to_the_aggregate_artifact_capacity_of_its_carrier(self):
        # Review of 16b59997d8: the producer (capture_artifacts) and the reader
        # (read_artifact_objects) hold one carrier's artifacts together to
        # finalized_proof.ARTIFACT_BYTES_PER_ROW, but _proof_bound held each
        # file to it alone, so startup set aside a proof neither could carry.
        # The capacity is lowered here to put three artifacts that each fit
        # past it together.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        proof = self._schema1_proof_files("aggregate-run", 1000)
        artifacts = [content for path, content in proof.items() if "/objects/" in path]
        total = sum(len(content) for content in artifacts)
        self.assertEqual(3, len(artifacts))
        self.assertLess(max(len(content) for content in proof.values()), total - 1)
        scorecard = {"scorecard/SCORECARD.md": "stranded\n", **proof}
        for label, files, message in (
                ("proof attachment", proof, mirror.PROOF_COMMIT_MESSAGE),
                ("scorecard carrying the proof", {"latest.json": self._scorecard_metadata(scorecard), **scorecard},
                 mirror.SCORECARD_COMMIT_MESSAGE)):
            with self.subTest(f"{label} one byte past the capacity"):
                head = self._strand(writer, files, message)
                try:
                    with mock.patch.object(mirror.finalized_proof, "ARTIFACT_BYTES_PER_ROW", total - 1), \
                            self.assertRaisesRegex(
                                mirror.MirrorRefused,
                                f"ledger writer has unpublished derived or unknown work at {head}; "
                                "retained for recovery"):
                        self.publish(publish_only=True)
                    self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
                    self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
                    self.assertEqual([row("published-first")], self.remote_rows())
                finally:
                    git(writer, "checkout", "-q", "--detach", base)
        # Exactly at the capacity the same proof is freed.
        stranded = self._strand(writer, proof, mirror.PROOF_COMMIT_MESSAGE)
        self.append("pending")
        with mock.patch.object(mirror.finalized_proof, "ARTIFACT_BYTES_PER_ROW", total), \
                contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.assertIn(f"WRITER FREED: a previous run left the writer on derived commit {stranded}",
                      stderr.getvalue())
        self.assertEqual(stranded, git(writer, "rev-parse", f"refs/heads/recovery/derived-{stranded}"))
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())

    def test_a_publication_refuses_a_writer_holding_bytes_its_head_does_not_store(self):
        # Review of 16b59997d8: the status check does not list ignored files,
        # and a clean filter or an assume-unchanged bit makes a changed file
        # compare clean. The checkout of newer main, the set-aside and the
        # scorecard regeneration then replaced such bytes, which are in no
        # commit and no recovery ref. Each case below passes the status
        # check; the publication must refuse when it starts and keep them.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        base = git(writer, "rev-parse", "HEAD")
        info = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-common-dir")) / "info"
        exclude, attributes = info / "exclude", info / "attributes"
        saved = {path: path.read_bytes() if path.exists() else None for path in (exclude, attributes)}

        def restore() -> None:
            for path, content in saved.items():
                if content is None:
                    path.unlink(missing_ok=True)
                else:
                    path.write_bytes(content)
            for key in ("filter.lower.clean", "filter.echo.clean", "filter.echo.smudge", "filter.proc.process",
                        *(f"filter.{driver}.{variable}" for driver in ("unset", "unspecified", "")
                          for variable in ("clean", "smudge"))):
                subprocess.run(["git", "-C", str(writer), "config", "--unset", key],
                               capture_output=True, check=False)

        def refusal(detail: str) -> str:
            return re.escape("ledger writer holds bytes its HEAD does not store, which a checkout or the "
                             f"scorecard regeneration could overwrite with no ref left: {detail}; "
                             "nothing was published")

        self.addCleanup(restore)
        info.mkdir(parents=True, exist_ok=True)
        parity = "scorecard/parity.json"
        operator = b'{"operator":"work in progress"}\n'
        with self.subTest("ignored file that the checkout of newer main would replace"):
            other = self.root / "other-host-parity"
            subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
            (other / parity).parent.mkdir(parents=True, exist_ok=True)
            (other / parity).write_bytes(b'{"published":true}\n')
            git(other, "add", parity)
            git(other, "commit", "-qm", "another host published parity")
            git(other, "push", "-q", "origin", "main")
            exclude.write_bytes((saved[exclude] or b"") + f"/{parity}\n".encode())
            (writer / parity).parent.mkdir(parents=True, exist_ok=True)
            (writer / parity).write_bytes(operator)
            self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
            self.append("pending-parity")
            with self.assertRaisesRegex(mirror.MirrorRefused, refusal(f"{parity!r} is untracked or ignored")):
                self.publish(publish_only=True)
            self.assertEqual(base, git(writer, "rev-parse", "HEAD"))
            self.assertEqual(operator, (writer / parity).read_bytes())
            self.assertEqual([row("published-first")], self.remote_rows())
            (writer / parity).unlink()
            restore()
            # Only the file held it: without it the publication goes ahead.
            self.assertEqual("published", self.publish(publish_only=True)["state"])
            self.assertEqual(b'{"published":true}\n', (writer / parity).read_bytes())
            self.assertEqual([row("published-first"), row("pending-parity")], self.remote_rows())
        published = git(writer, "rev-parse", "HEAD")
        scorecard = {"scorecard/SCORECARD.md": "stranded\n"}
        hidden = "scorecard/SCORECARD.md"
        # Review of 1335ba29ec: a smudge filter is an arbitrary program. One
        # that prints the file on disk makes `git cat-file --filters` match
        # any edit, so matching a filter's output proves nothing is stored.
        stored = self.root / "stored-scorecard"
        stored.write_bytes(b"stranded\n")
        # Review of 8f2b7800de: git check-attr prints a driver named "unset"
        # or "unspecified" exactly as it prints those attribute states, so a
        # check of the attribute let such a smudge program run. No filter
        # program may run in the comparison at all; this one leaves a marker.
        smudged = self.root / "smudged"

        def driver(name: str, attribute: str) -> None:
            git(writer, "config", f"filter.{name}.clean", f"cat {shlex.quote(str(stored))}")
            git(writer, "config", f"filter.{name}.smudge",
                f"touch {shlex.quote(str(smudged))} && cat {shlex.quote(str(writer / hidden))}")
            attributes.write_bytes(attribute.encode())

        for label, hide in (
                ("edit hidden by a clean filter",
                 lambda: (git(writer, "config", "filter.lower.clean", "tr A-Z a-z"),
                          attributes.write_bytes(f"/{hidden} filter=lower\n".encode()))),
                ("edit hidden by an assume-unchanged bit",
                 lambda: git(writer, "update-index", "--assume-unchanged", hidden)),
                ("edit hidden by a smudge filter that prints the file on disk",
                 lambda: (git(writer, "config", "filter.echo.clean", f"cat {shlex.quote(str(stored))}"),
                          git(writer, "config", "filter.echo.smudge",
                              f"cat {shlex.quote(str(writer / hidden))}"),
                          attributes.write_bytes(f"/{hidden} filter=echo\n".encode()))),
                ("edit hidden by a smudge filter driver named unset",
                 lambda: driver("unset", f"/{hidden} filter=unset\n")),
                ("edit hidden by a smudge filter driver named unspecified",
                 lambda: driver("unspecified", f"/{hidden} filter=unspecified\n")),
                ("edit hidden by a smudge filter driver named unset through an attribute macro",
                 lambda: driver("unset", f"[attr]keep-bytes filter=unset\n/{hidden} keep-bytes\n")),
                # Review of 6f4e0db6e1: an attribute set to nothing selects the
                # driver with an empty name, [filter ""], whose smudge runs.
                ("edit hidden by a smudge filter driver with an empty name",
                 lambda: driver("", f"/{hidden} filter=\n")),
                # A driver can name a long-running process instead of a smudge
                # command; the bit keeps git status from starting it.
                ("edit under an assume-unchanged bit at a path with a long-running process filter driver",
                 lambda: (git(writer, "update-index", "--assume-unchanged", hidden),
                          git(writer, "config", "filter.proc.process",
                              f"touch {shlex.quote(str(smudged))}; exit 1"),
                          attributes.write_bytes(f"/{hidden} filter=proc\n".encode())))):
            with self.subTest(label):
                head = self._strand(writer, {"latest.json": self._scorecard_metadata(scorecard), **scorecard},
                                    mirror.SCORECARD_COMMIT_MESSAGE)
                try:
                    hide()
                    (writer / hidden).write_bytes(b"STRANDED\n")
                    self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                    if "smudge" in label:
                        # What a checkout writes, per Git, is the edit itself.
                        self.assertEqual(b"STRANDED\n", subprocess.run(
                            ["git", "-C", str(writer), "cat-file", "--filters", f"HEAD:{hidden}"],
                            capture_output=True, check=True).stdout)
                    if "named" in label:
                        # Git reports the driver name exactly as it reports the attribute state.
                        name = "unspecified" if "unspecified" in label else "unset"
                        self.assertEqual(f"{hidden}: filter: {name}",
                                         git(writer, "check-attr", "filter", "--", hidden))
                        self.assertTrue(smudged.exists())
                        smudged.unlink()
                    if "empty name" in label:
                        self.assertEqual(f"{hidden}: filter:", git(writer, "check-attr", "filter", "--", hidden))
                        self.assertTrue(smudged.exists())
                        smudged.unlink()
                    if "process" in label:
                        # Git starts the driver for what a checkout writes; it
                        # leaves the marker and fails the handshake.
                        self.assertNotEqual(0, subprocess.run(
                            ["git", "-C", str(writer), "cat-file", "--filters", f"HEAD:{hidden}"],
                            capture_output=True, check=False).returncode)
                        self.assertTrue(smudged.exists())
                        smudged.unlink()
                    with self.assertRaisesRegex(mirror.MirrorRefused,
                                                refusal(f"{hidden!r} differs from the bytes HEAD stores")):
                        self.publish(publish_only=True)
                    self.assertFalse(smudged.exists(), "a filter program ran during the publication")
                    self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
                    self.assertEqual(b"STRANDED\n", (writer / hidden).read_bytes())
                    self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
                    self.assertEqual([row("published-first"), row("pending-parity")], self.remote_rows())
                finally:
                    # Drivers first: writing the index can run one on a
                    # racily clean entry.
                    restore()
                    git(writer, "update-index", "--no-assume-unchanged", hidden)
                    git(writer, "checkout", "-q", "-f", "--detach", published)
        # Review of 51a5a5cd1c: under core.ignoreCase the untracked listing
        # hid a name differing only in case from a stored one, and a
        # populated directory at a submodule path was never read.
        variant = "scorecard/scorecard.md"
        with self.subTest("name differing only in case from a stored one, under core.ignoreCase"):
            git(writer, "config", "core.ignoreCase", "true")
            try:
                (writer / variant).write_bytes(operator)
                self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                with self.assertRaisesRegex(mirror.MirrorRefused, refusal(f"{variant!r} is untracked or ignored")):
                    self.publish(publish_only=True)
                self.assertEqual(published, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(operator, (writer / variant).read_bytes())
                self.assertEqual([row("published-first"), row("pending-parity")], self.remote_rows())
            finally:
                (writer / variant).unlink(missing_ok=True)
                git(writer, "config", "--unset", "core.ignoreCase")
        module = "vendored"
        with self.subTest("populated directory at a submodule path"):
            git(writer, "update-index", "--add", "--cacheinfo", f"160000,{published},{module}")
            git(writer, "commit", "-qm", mirror.SCORECARD_COMMIT_MESSAGE)
            head = git(writer, "rev-parse", "HEAD")
            try:
                (writer / module).mkdir()
                (writer / module / "notes.txt").write_bytes(operator)
                self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                with self.assertRaisesRegex(mirror.MirrorRefused, refusal(
                        f"{module!r} is a submodule checkout whose bytes this publication cannot prove stored")):
                    self.publish(publish_only=True)
                self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(operator, (writer / module / "notes.txt").read_bytes())
                self.assertEqual("", git(writer, "for-each-ref", "refs/heads/recovery/"))
                self.assertEqual([row("published-first"), row("pending-parity")], self.remote_rows())
                # With the directory empty, nothing is held there.
                (writer / module / "notes.txt").unlink()
                self.assertEqual([], mirror._unstored_bytes(writer))
            finally:
                (writer / module / "notes.txt").unlink(missing_ok=True)
                if (writer / module).is_dir():
                    (writer / module).rmdir()
                git(writer, "checkout", "-q", "-f", "--detach", published)

    def test_a_publication_goes_ahead_over_bytes_a_checkout_writes_or_leaves_absent(self):
        # Review of 51a5a5cd1c: a file a checkout writes with CRLF under
        # core.autocrlf, and a stored file a skip-worktree bit leaves absent,
        # hold no bytes HEAD does not store. Refusing them stalled every
        # publication from such a writer.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        git(writer, "config", "core.autocrlf", "true")
        converted = "scorecard/SCORECARD.md"
        (writer / converted).unlink()
        git(writer, "checkout", "--", converted)
        self.assertEqual(b"legacy scorecard\r\n", (writer / converted).read_bytes())
        absent = "scorecard/cells.json"
        git(writer, "update-index", "--skip-worktree", absent)
        (writer / absent).unlink()
        self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
        self.assertEqual([], mirror._unstored_bytes(writer))
        # An edit a checkout would not write is still refused.
        (writer / converted).write_bytes(b"legacy scorecard edited\r\n")
        self.assertEqual([f"{converted!r} differs from the bytes HEAD stores"], mirror._unstored_bytes(writer))
        git(writer, "checkout", "--", converted)
        # The publication goes ahead with the skip-worktree bit still set and
        # the file still absent (review of 8f2b7800de: the previous version
        # of this test cleared the bit and checked the file out first). `git
        # add` does not stage a path under that bit, so the publication
        # clears it (test_a_publication_clears_index_bits_that_git_add_does_not_honour)
        # and writes HEAD's bytes back: the absent file held nothing.
        self.append("pending")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())
        self.assertEqual(b"legacy scorecard\r\n", (writer / converted).read_bytes())
        self.assertEqual(f"H {absent}", git(writer, "ls-files", "-v", "--", absent))
        self.assertEqual(b'{"legacy":"preserve"}\r\n', (writer / absent).read_bytes())
        self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))

    def test_bytes_the_writer_gains_while_the_scorecard_tool_runs_withhold_the_scorecard(self):
        # Review of 51a5a5cd1c: the render takes about 1000 s in production,
        # and the status check after it lists no ignored file. An ignored
        # scorecard/cells.json written into the writer meanwhile, beside a
        # history stored in parts, was deleted by the regeneration with no
        # ref left. The scorecard is withheld and the file kept.
        self.append("first-parts")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("", git(writer, "ls-tree", "HEAD", "--", "scorecard/cells.json"))
        before = self.published_projection()
        exclude = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-common-dir")) / "info" / "exclude"
        exclude.parent.mkdir(parents=True, exist_ok=True)
        exclude.write_bytes((exclude.read_bytes() if exclude.exists() else b"") + b"/scorecard/cells.json\n")
        operator = b'{"operator":"draft"}\n'
        self.set_tool(f"w=pathlib.Path({str(writer)!r})\n"
                      f"(w/'scorecard'/'cells.json').write_bytes({operator!r})\n"
                      "(p/'cells.json').write_bytes(b'{\"rendered\":\"in the window\"}\\n')\n"
                      "(p/'SCORECARD.md').write_text('rendered in the window\\n')\n")
        self.append("second-parts")
        report = self.publish()
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual("failed", report["scorecard"]["state"])
        self.assertIn("the ledger writer gained bytes its HEAD does not store while the scorecard tool ran; "
                      "nothing was copied into the ledger writer, which is retained for recovery: "
                      "'scorecard/cells.json' is untracked or ignored", report["scorecard"]["error"])
        self.assertEqual(operator, (writer / "scorecard/cells.json").read_bytes())
        self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
        self.assertEqual(before, self.published_projection())
        self.assertEqual([row("first-parts"), row("second-parts")], self.remote_rows())

    def test_bytes_written_after_the_last_check_are_moved_aside_not_replaced(self):
        # Review of 1335ba29ec: a process without the publication lock can
        # write into the writer after the last _unstored_bytes check, and the
        # regeneration and latest.json then replaced its bytes with no ref
        # left. Each replacement now moves the file there aside first and
        # keeps bytes HEAD does not store; a file that appears after the move
        # is not replaced. The check is wrapped here to write just after the
        # one that follows the render. Review of 6f4e0db6e1: deleting stored
        # bytes at once under a write lease lost a write made by reopening
        # the file through a handle (/proc/self/fd), and refused every
        # replacement while a reader had the file open. Every replaced file
        # is now kept where it was moved, recorded when it held stored bytes,
        # and a later publication deletes it once it is unchanged and closed.
        # Review of 36da12785d: no check before an unlink stops an open made
        # after it (a rename or link breaks no lease), and a write through
        # that open was lost with the name. Nothing moved aside is unlinked
        # now: a later publication empties it, truncating it through its
        # leased descriptor, so an open waiting on the lease gets the emptied
        # file, which keeps its name.
        self.append("first-parts")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        self.assertEqual("", git(writer, "ls-tree", "HEAD", "--", "scorecard/cells.json"))
        info = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-common-dir")) / "info"
        attributes = info / "attributes"
        saved_attributes = attributes.read_bytes() if attributes.exists() else None

        def displaced() -> list[Path]:
            return sorted(writer.parent.glob(f"{writer.name}.displaced-*"))

        def holding(name: str) -> list[Path]:
            return [retained for retained in displaced() if os.path.lexists(retained / "files" / name)]

        def recorded(retained: Path) -> list[dict]:
            record = retained / "displaced.json"
            return json.loads(record.read_text())["files"] if record.exists() else []

        def entry(name: str, data: bytes) -> dict:
            return {"path": name, "sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}

        def later() -> float:
            return time.time() + mirror.DISPLACED_RETENTION_SECONDS + 1

        def prune(when: float) -> str:
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                mirror.prune_displaced(writer, now=when)
            return stderr.getvalue()

        def emptied(retained: Path, names: list[str]) -> None:
            # Emptied, then removed with the record and every directory left
            # empty: the retention directory itself is gone.
            self.assertNotEqual([], names)
            self.assertFalse(os.path.lexists(retained), retained)

        racer = ("import os, sys\n"
                 "descriptor = os.open(sys.argv[1], os.O_WRONLY)\n"
                 "os.write(descriptor, b'racer bytes\\n')\n")

        # The first publication replaced the seed's scorecard and removed its
        # single cells.json, all stored bytes: each is kept and recorded,
        # left alone while young, and emptied once old, unchanged and closed.
        first = displaced()
        self.assertNotEqual([], first)
        names: dict[Path, list[str]] = {}
        for retained in first:
            files = recorded(retained)
            self.assertNotEqual([], files)
            for item in files:
                self.assertEqual(entry(item["path"], (retained / "files" / item["path"]).read_bytes()), item)
            names[retained] = [item["path"] for item in files]
        self.assertEqual("", prune(time.time()))
        self.assertEqual(first, displaced())
        self.assertRegex(prune(later()), r"emptied \d+ unchanged file\(s\) earlier publications moved aside")
        self.assertEqual([], displaced())
        for retained, paths in names.items():
            emptied(retained, paths)
        self.assertEqual("", prune(later()))

        before = self.published_projection()
        marker = self.root / "rendered"
        operator = b'{"operator":"draft"}\n'
        real = mirror._unstored_bytes
        target: dict[str, str] = {}

        def racing(path: Path) -> list[str]:
            found = real(path)
            if marker.exists():
                marker.unlink()
                (path / target["name"]).write_bytes(operator)
            return found

        def render(label: str) -> None:
            document = json.dumps({"rendered": label}).encode() + b"\n"
            self.set_tool(f"pathlib.Path({str(marker)!r}).write_bytes(b'')\n"
                          f"(p/'cells.json').write_bytes({document!r})\n"
                          f"(p/'SCORECARD.md').write_text({'rendered for ' + label + chr(10)!r})\n")

        rows = [row("first-parts")]

        def reset() -> None:
            # The operator takes the bytes back; the writer returns to HEAD.
            for retained in displaced():
                shutil.rmtree(retained)
            git(writer, "checkout", "-q", "-f", "--detach", "HEAD")
            git(writer, "clean", "-fdq")
            for ref in git(writer, "for-each-ref", "--format=%(refname)", "refs/heads/recovery/").split():
                git(writer, "update-ref", "-d", ref)

        # scorecard/cells.json is removed (the history is stored in parts);
        # the other two are overwritten.
        for index, name in enumerate(("scorecard/cells.json", "scorecard/SCORECARD.md", "latest.json")):
            with self.subTest(f"{name} written after the last check"):
                target["name"] = name
                render(name)
                self.append(f"pending-{index}")
                rows.append(row(f"pending-{index}"))
                try:
                    with mock.patch.object(mirror, "_unstored_bytes", racing):
                        report = self.publish()
                    self.assertFalse(marker.exists())
                    self.assertEqual("derived-failed", report["state"], report)
                    self.assertRegex(report["scorecard"]["error"], re.escape(
                        f"ledger writer path {name} held bytes its HEAD does not store, written while this "
                        f"publication ran; they were moved to {writer.parent}/{writer.name}.displaced-")
                        + "[^/]+/files/" + re.escape(f"{name} and nothing more was written; what was written "
                                                     "is kept for recovery"))
                    [retained] = holding(name)
                    self.assertEqual(operator, (retained / "files" / name).read_bytes())
                    # Unstored bytes are not recorded, so no publication deletes them.
                    self.assertNotIn(name, [item["path"] for item in recorded(retained)])
                    prune(later())
                    self.assertEqual(operator, (retained / "files" / name).read_bytes())
                    # The render output written before the refusal is set
                    # aside when the scorecard step ends (b0ad620e92), so the
                    # writer holds exactly HEAD's bytes.
                    self.assertRegex(" ".join(report["writer_cleaned"]), "saved as refs/heads/recovery/render-")
                    self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                    self.assertEqual([], mirror._unstored_bytes(writer))
                    self.assertEqual(git(self.remote, "rev-parse", "main"), git(writer, "rev-parse", "HEAD"))
                    self.assertEqual(before, self.published_projection())
                    self.assertEqual(rows, self.remote_rows())
                finally:
                    marker.unlink(missing_ok=True)
                    reset()
        scorecard = "scorecard/SCORECARD.md"
        stored = (writer / scorecard).read_bytes()
        with self.subTest("file that appears at its path after the move"):
            real_holds = mirror._holds_stored_bytes

            def appearing(path: Path, name: bytes, *rest) -> bool:
                held = real_holds(path, name, *rest)
                (path / os.fsdecode(name)).write_bytes(operator)
                return held

            try:
                with mock.patch.object(mirror, "_holds_stored_bytes", appearing), self.assertRaisesRegex(
                        mirror.MirrorRefused, re.escape(
                            f"ledger writer path {scorecard} was written by another process while this "
                            "publication replaced it; it was not replaced and nothing more was written")):
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                self.assertEqual(operator, (writer / scorecard).read_bytes())
                # The stored bytes that were moved aside are kept and
                # recorded, and emptied once old.
                [retained] = displaced()
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
                self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                prune(later())
                emptied(retained, [scorecard])
            finally:
                reset()
        # Review of 8f2b7800de: the move freezes the name, not the file. A
        # process that still has it open can write after the comparison.
        # The replacement goes ahead; the late write lands in the kept file.
        with self.subTest("file another process still has open for writing"):
            descriptor = os.open(writer / scorecard, os.O_WRONLY)
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                self.assertEqual(b"replacement\n", (writer / scorecard).read_bytes())
                os.write(descriptor, b"late edit")
                [retained] = displaced()
                late = b"late edit" + stored[len(b"late edit"):]
                self.assertEqual(late, (retained / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
                self.assertIn(f"KEPT 1 file(s) an earlier publication moved to {retained}/files: {scorecard} is "
                              "open in another process, which could still write to it (no write lease: ",
                              prune(later()))
                os.close(descriptor)
                descriptor = -1
                self.assertIn(f"{scorecard} was written after it was moved there", prune(later()))
                self.assertEqual(late, (retained / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
            finally:
                if descriptor >= 0:
                    os.close(descriptor)
                reset()
        # Review of 6f4e0db6e1: an O_PATH handle does not stop a write lease,
        # and reopening the file through it broke the lease after the check,
        # so the unlink dropped what the reopened descriptor then wrote.
        with self.subTest("file reopened for writing through a handle held before the move"):
            handle = os.open(writer / scorecard, os.O_PATH)
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                reopened = os.open(f"/proc/self/fd/{handle}", os.O_WRONLY)
                os.write(reopened, b"late edit")
                os.close(reopened)
            finally:
                os.close(handle)
            try:
                [retained] = displaced()
                late = b"late edit" + stored[len(b"late edit"):]
                self.assertEqual(late, (retained / "files" / scorecard).read_bytes())
                self.assertIn(f"{scorecard} was written after it was moved there", prune(later()))
                self.assertEqual(late, (retained / "files" / scorecard).read_bytes())
            finally:
                reset()
        with self.subTest("file opened by another process while it was compared"):
            real_sha256 = mirror._sha256_through
            opened: list[bool] = []
            retained = Path()

            def opening(descriptor: int) -> tuple[str, int]:
                # The comparison holds a write lease, so this open waits for
                # it (here, without blocking, it fails) and breaks it.
                with self.assertRaises(BlockingIOError):
                    os.open(retained / "files" / scorecard, os.O_RDONLY | os.O_NONBLOCK)
                opened.append(True)
                return real_sha256(descriptor)

            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                with mock.patch.object(mirror, "_sha256_through", opening):
                    self.assertIn(f"{scorecard} was opened by another process while it was compared",
                                  prune(later()))
                self.assertEqual([True], opened)
                self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                prune(later())
                emptied(retained, [scorecard])
            finally:
                reset()
        # Review of 36da12785d: an open made after the last lease check,
        # just before the unlink, waited for the lease, then wrote into the
        # file once it had no name. The emptying holds the lease until the
        # file is truncated, so such an open returns afterwards, and what is
        # written through it stays in the file. (The unlink is wrapped too,
        # to open there under an implementation that unlinks.)
        with self.subTest("file another process opens just before it is emptied"):
            real_truncate, real_unlink, real_fcntl = mirror._truncate_leased, os.unlink, fcntl.fcntl
            racers: list[subprocess.Popen] = []
            retained = Path()

            def race() -> None:
                racers.append(subprocess.Popen([sys.executable, "-c", racer,
                                                str(retained / "files" / scorecard)]))

            def truncating(descriptor: int, deadline: float) -> int:
                race()
                waited = time.monotonic() + 30
                while real_fcntl(descriptor, fcntl.F_GETLEASE) == fcntl.F_WRLCK:
                    self.assertLess(time.monotonic(), waited, "the racing open never broke the lease")
                    time.sleep(0.01)
                return real_truncate(descriptor, deadline)

            def unlinking(path, *rest, **options) -> None:
                if os.path.basename(os.fsdecode(path)) == Path(scorecard).name:
                    race()
                    time.sleep(1)
                real_unlink(path, *rest, **options)

            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                with mock.patch.object(mirror, "_truncate_leased", truncating), \
                        mock.patch.object(mirror.os, "unlink", unlinking):
                    pruned = prune(later())
                self.assertEqual(1, len(racers))
                self.assertEqual(0, racers[0].wait(timeout=60))
                self.assertIn(f"{scorecard} was opened by another process as it was emptied; what that "
                              "process writes stays in the file, which no publication empties again", pruned)
                self.assertEqual(b"racer bytes\n", (retained / "files" / scorecard).read_bytes())
                self.assertEqual([], recorded(retained))
                prune(later())
                self.assertEqual(b"racer bytes\n", (retained / "files" / scorecard).read_bytes())
            finally:
                for process in racers:
                    if process.poll() is None:
                        process.kill()
                        process.wait()
                reset()
        # Review of 36da12785d: a file renamed over the kept one after the
        # inode check, just before the unlink, was the file unlinked. The
        # truncation goes through the descriptor, so it reaches only the
        # file that was compared.
        with self.subTest("file renamed over the kept one just before it is emptied"):
            real_truncate, real_unlink = mirror._truncate_leased, os.unlink
            retained = Path()

            def rename_over() -> None:
                other = retained / "operator-bytes"
                other.write_bytes(operator)
                os.rename(other, retained / "files" / scorecard)

            def truncating(descriptor: int, deadline: float) -> int:
                rename_over()
                return real_truncate(descriptor, deadline)

            def unlinking(path, *rest, **options) -> None:
                if os.path.basename(os.fsdecode(path)) == Path(scorecard).name:
                    rename_over()
                real_unlink(path, *rest, **options)

            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                with mock.patch.object(mirror, "_truncate_leased", truncating), \
                        mock.patch.object(mirror.os, "unlink", unlinking):
                    prune(later())
                self.assertEqual(operator, (retained / "files" / scorecard).read_bytes())
                prune(later())
                self.assertEqual(operator, (retained / "files" / scorecard).read_bytes())
            finally:
                reset()
        # Review of 0287e858e4: an open waiting on the lease goes ahead
        # lease-break-time seconds after its break began, so a truncation
        # delayed past that (the publication stopped or descheduled after
        # its last check) erased what that open had written by then. The
        # truncation now runs in a child the kernel ends at a deadline well
        # inside that time. Here lease-break-time reads as 4 s, so the
        # deadline is 1 s after the lease was last confirmed, and the child
        # is held past it: asleep or stopped before it truncates, held
        # between reading the clock and arming its timer, or started late.
        # The file is kept whole, and the open that waited meanwhile writes
        # into it once the lease is released.
        stalls = {
            "asleep before it truncates": ("os.ftruncate(", "time.sleep(3)\n"),
            "stopped before it truncates": ("os.ftruncate(", "os.kill(os.getpid(), signal.SIGSTOP)\n"),
            "held between reading the clock and arming its timer":
                ("signal.setitimer(signal.ITIMER_REAL, remaining)", "time.sleep(1.2)\n"),
            "started after its deadline": ("remaining = ", "time.sleep(1.2)\n"),
        }
        for label, (anchor, stall) in stalls.items():
            with self.subTest(f"file whose emptying is {label}"):
                self.assertEqual(1, mirror._TRUNCATE_PROGRAM.count(anchor))
                program = mirror._TRUNCATE_PROGRAM.replace(anchor, stall + anchor)
                real_truncate = mirror._truncate_leased
                racers = []
                held: list[tuple[int, float]] = []
                retained = Path()

                def stalled(descriptor: int, deadline: float) -> tuple[int, float | None]:
                    racers.append(subprocess.Popen([sys.executable, "-c", racer,
                                                    str(retained / "files" / scorecard)]))
                    waited = time.monotonic() + 30
                    while fcntl.fcntl(descriptor, fcntl.F_GETLEASE) == fcntl.F_WRLCK:
                        self.assertLess(time.monotonic(), waited, "the racing open never broke the lease")
                        time.sleep(0.01)
                    status, ended = real_truncate(descriptor, deadline)
                    held.append((status, time.monotonic() - deadline))
                    self.assertIsNone(ended)
                    return status, ended

                try:
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                    [retained] = displaced()
                    with mock.patch.object(mirror, "_lease_break_seconds", return_value=4), \
                            mock.patch.object(mirror, "_TRUNCATE_PROGRAM", program), \
                            mock.patch.object(mirror, "_truncate_leased", stalled):
                        pruned = prune(later())
                    self.assertEqual(0, racers[0].wait(timeout=60))
                    [(status, late)] = held
                    self.assertIn(status, (3, -signal.SIGALRM))
                    # Ended by its deadline (with some scheduling slack),
                    # not when its stall would have finished; a stopped
                    # child is killed once the grace after it has passed.
                    stopped = "SIGSTOP" in stall
                    self.assertLess(late, 1.5 + (mirror._TRUNCATE_GRACE_SECONDS if stopped else 0))
                    if stopped:
                        self.assertGreater(late, mirror._TRUNCATE_GRACE_SECONDS - 0.5)
                    self.assertIn(f"KEPT 1 file(s) an earlier publication moved to {retained}/files: {scorecard} "
                                  "was not emptied: its deadline passed before the truncation began", pruned)
                    self.assertEqual(b"racer bytes\n" + stored[len(b"racer bytes\n"):],
                                     (retained / "files" / scorecard).read_bytes())
                    self.assertEqual([entry(scorecard, stored)], recorded(retained))
                    self.assertIn(f"{scorecard} was written after it was moved there", prune(later()))
                finally:
                    for process in racers:
                        if process.poll() is None:
                            process.kill()
                            process.wait()
                    reset()
        # A break that began after the last confirmation, with the
        # truncation not seen to end until within a second of
        # lease-break-time after it or later, may have let a waiting open
        # write before the truncation took effect: the note says so rather
        # than that its bytes stay. Review of a3f62cfee1: the time was this
        # process's own, read after the child exited, and the test delayed
        # only that; the child now reads the clock once the truncation
        # returns. Held inside the truncation here means its timer disarmed
        # and 3.2 s slept just before it (the kernel holding it there); a
        # delay in this process alone after a prompt truncation is not late.
        late_cases = {
            "held inside its truncation past lease-break-time": (True, 0.0),
            "seen to end late only by this process": (False, 3.2),
        }
        for label, (held_inside, after) in late_cases.items():
            with self.subTest(f"file emptied with an open waiting, {label}"):
                anchor = "os.ftruncate("
                self.assertEqual(1, mirror._TRUNCATE_PROGRAM.count(anchor))
                program = (mirror._TRUNCATE_PROGRAM.replace(
                    anchor, "signal.setitimer(signal.ITIMER_REAL, 0)\ntime.sleep(3.2)\n" + anchor)
                    if held_inside else mirror._TRUNCATE_PROGRAM)
                real_truncate = mirror._truncate_leased
                retained = Path()
                reported: list[tuple[int, float | None]] = []

                def slow(descriptor: int, deadline: float) -> tuple[int, float | None]:
                    with self.assertRaises(BlockingIOError):
                        os.open(retained / "files" / scorecard, os.O_RDONLY | os.O_NONBLOCK)
                    result = real_truncate(descriptor, deadline)
                    reported.append(result)
                    time.sleep(after)
                    return result

                try:
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                    [retained] = displaced()
                    with mock.patch.object(mirror, "_lease_break_seconds", return_value=4), \
                            mock.patch.object(mirror, "_TRUNCATE_PROGRAM", program), \
                            mock.patch.object(mirror, "_truncate_leased", slow):
                        pruned = prune(later())
                    [(status, ended)] = reported
                    self.assertEqual(0, status)
                    self.assertIsNotNone(ended)
                    if held_inside:
                        late = re.search(re.escape(
                            f"{scorecard} was opened by another process while it was emptied, and the truncation "
                            "was not seen to end until ") + r"(\d+\.\d) s" + re.escape(
                            " after the lease was last confirmed, within 1 s of or past the 4 s lease-break-time: "
                            "bytes that process wrote before the truncation took effect may be gone; what it "
                            "writes after it stays in the file"), pruned)
                        self.assertIsNotNone(late, pruned)
                        self.assertGreaterEqual(float(late.group(1)), 3.0)
                    else:
                        self.assertIn(f"{scorecard} was opened by another process as it was emptied; what that "
                                      "process writes stays in the file, which no publication empties again",
                                      pruned)
                        self.assertNotIn("lease-break-time", pruned)
                    emptied(retained, [scorecard])
                finally:
                    reset()
        # The deadline is a fraction of lease-break-time, so the emptying
        # needs that value, and enough of it to leave a margin.
        missing = self.root / "no-lease-break-time"
        for label, patch, why in (
                ("unreadable", mock.patch.object(mirror, "LEASE_BREAK_TIME", missing),
                 f"could not be emptied safely: {missing} could not be read"),
                ("under its minimum", mock.patch.object(mirror, "_lease_break_seconds", return_value=3),
                 f"could not be emptied safely: {mirror.LEASE_BREAK_TIME} is 3 s, under 4 s")):
            with self.subTest(f"file on a host whose lease-break-time is {label}"):
                try:
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                    [retained] = displaced()
                    with patch:
                        self.assertIn(f"{scorecard} {why}", prune(later()))
                    self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                    self.assertEqual([entry(scorecard, stored)], recorded(retained))
                    prune(later())
                    emptied(retained, [scorecard])
                finally:
                    reset()
        # A file renamed over the kept one while it is compared opens
        # nothing, so the lease holds; the unlink must not take that file.
        with self.subTest("file renamed over the kept one while it was compared"):
            real_sha256 = mirror._sha256_through
            retained = Path()

            def replacing(descriptor: int) -> tuple[str, int]:
                other = retained / "operator-bytes"
                other.write_bytes(operator)
                os.rename(other, retained / "files" / scorecard)
                return real_sha256(descriptor)

            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                with mock.patch.object(mirror, "_sha256_through", replacing):
                    self.assertIn(f"{scorecard} was replaced by another file while it was compared",
                                  prune(later()))
                self.assertEqual(operator, (retained / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
                self.assertIn(f"{scorecard} was written after it was moved there", prune(later()))
                self.assertEqual(operator, (retained / "files" / scorecard).read_bytes())
            finally:
                reset()
        # Truncating a file empties every name it has, so a kept file with a
        # second name (a hard link someone made to it) is left alone.
        with self.subTest("file with a second name"):
            second = self.root / "second-name"
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                os.link(retained / "files" / scorecard, second)
                self.assertIn(f"{scorecard} has 2 names, and emptying it would empty the others", prune(later()))
                self.assertEqual(stored, second.read_bytes())
                self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
                second.unlink()
                prune(later())
                emptied(retained, [scorecard])
            finally:
                second.unlink(missing_ok=True)
                reset()
        # A symbolic link holds its target as its bytes: one HEAD does not
        # store at the path stops the publication and is kept, not unlinked.
        with self.subTest("symbolic link written at a path after the last check"):
            try:
                (writer / scorecard).unlink()
                os.symlink("operator-target", writer / scorecard)
                with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(
                        f"ledger writer path {scorecard} held bytes its HEAD does not store")):
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                self.assertFalse(os.path.lexists(writer / scorecard))
                [retained] = displaced()
                self.assertEqual("operator-target", os.readlink(retained / "files" / scorecard))
                self.assertEqual([], recorded(retained))
                prune(later())
                self.assertEqual("operator-target", os.readlink(retained / "files" / scorecard))
            finally:
                reset()
        with self.subTest("file on a filesystem that grants no write lease"):
            real_fcntl = fcntl.fcntl

            def no_lease(descriptor, command, *rest):
                if command == fcntl.F_SETLEASE and rest == (fcntl.F_WRLCK,):
                    raise OSError(errno.EINVAL, os.strerror(errno.EINVAL))
                return real_fcntl(descriptor, command, *rest)

            try:
                with mock.patch.object(mirror.fcntl, "fcntl", no_lease):
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                    self.assertEqual(b"replacement\n", (writer / scorecard).read_bytes())
                    [retained] = displaced()
                    self.assertIn(f"{scorecard} could not be leased (fcntl: Invalid argument), so another "
                                  "process could still write to it", prune(later()))
                self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                prune(later())
                emptied(retained, [scorecard])
            finally:
                reset()
        with self.subTest("retention directory with no record, a broken record, or a young one"):
            unrecorded = writer.parent / f"{writer.name}.displaced-unrecorded"
            broken = writer.parent / f"{writer.name}.displaced-broken"
            escaping = writer.parent / f"{writer.name}.displaced-escaping"
            for retained in (unrecorded, broken, escaping):
                (retained / "files" / scorecard).parent.mkdir(parents=True)
                (retained / "files" / scorecard).write_bytes(stored)
            (broken / "displaced.json").write_text('{"displaced_at": 0, "files": [')
            # A record naming a path outside its files directory.
            (escaping / "outside").write_bytes(stored)
            (escaping / "displaced.json").write_text(json.dumps(
                {"displaced_at": 0, "files": [entry("../outside", stored)]}))
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [young] = set(displaced()) - {unrecorded, broken, escaping}
                self.assertIn("../outside is not a path inside its retention directory", prune(time.time()))
                self.assertEqual(stored, (young / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(young))
                self.assertIn("../outside is not a path inside its retention directory", prune(later()))
                # Only the young directory had a usable record naming files
                # it could empty, so only it is removed.
                self.assertEqual(sorted([broken, escaping, unrecorded]), displaced())
                emptied(young, [scorecard])
                for retained in (unrecorded, broken, escaping):
                    self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                self.assertEqual(stored, (escaping / "outside").read_bytes())
            finally:
                reset()
        # Review of 36da12785d: the record was rewritten through a fixed
        # temporary name, following a symbolic link planted there.
        with self.subTest("link planted at the record's temporary name"):
            victim = self.root / "record-victim"
            victim.write_bytes(b"victim bytes\n")
            reader = os.open(writer / scorecard, os.O_RDONLY)
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                for planted in (".displaced.json.tmp", ".displaced.json.planted.tmp"):
                    os.symlink(victim, retained / planted)
                # The reader keeps the file, so the record is rewritten.
                with mock.patch.object(mirror, "_record_name", create=True,
                                       side_effect=[".displaced.json.planted.tmp", ".displaced.json.fresh.tmp"]):
                    self.assertIn(f"{scorecard} is open in another process", prune(later()))
                self.assertEqual(b"victim bytes\n", victim.read_bytes())
                self.assertFalse(os.path.islink(retained / "displaced.json"))
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
                for planted in (".displaced.json.tmp", ".displaced.json.planted.tmp"):
                    self.assertEqual(str(victim), os.readlink(retained / planted))
                self.assertFalse(os.path.lexists(retained / ".displaced.json.fresh.tmp"))
            finally:
                os.close(reader)
                reset()
        # Review of 36da12785d: the retention directory was used by path, so
        # one renamed away and replaced by a link while it was pruned led
        # the rest of the prune elsewhere.
        with self.subTest("retention directory renamed away and replaced by a link while it is pruned"):
            outside, moved = self.root / "outside-retention", writer.parent / "moved-retention"
            (outside / "files" / "scorecard").mkdir(parents=True)
            real_sha256 = mirror._sha256_through
            retained = Path()

            def swapping(descriptor: int) -> tuple[str, int]:
                os.rename(retained, moved)
                os.symlink(outside, retained)
                return "0" * 64, real_sha256(descriptor)[1]

            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                with mock.patch.object(mirror, "_sha256_through", swapping):
                    self.assertIn(f"{scorecard} was written after it was moved there", prune(later()))
                self.assertEqual([outside / "files", outside / "files" / "scorecard"], sorted(outside.rglob("*")))
                self.assertEqual([entry(scorecard, stored)], json.loads((moved / "displaced.json").read_text())["files"])
                self.assertEqual(stored, (moved / "files" / scorecard).read_bytes())
            finally:
                if retained.is_symlink():
                    retained.unlink()
                shutil.rmtree(moved, ignore_errors=True)
                shutil.rmtree(outside, ignore_errors=True)
                reset()
        # A retention directory name that is itself a symbolic link is not
        # followed: the record and bytes it leads to are not the prune's.
        with self.subTest("retention directory name that is a link"):
            outside = self.root / "outside-linked-retention"
            linked = writer.parent / f"{writer.name}.displaced-linked"
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                shutil.copytree(retained, outside, symlinks=True)
                shutil.rmtree(retained)
                os.symlink(outside, linked)
                self.assertEqual([linked], displaced())
                self.assertEqual("", prune(later()))
                self.assertEqual(stored, (outside / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)],
                                 json.loads((outside / "displaced.json").read_text())["files"])
            finally:
                linked.unlink(missing_ok=True)
                shutil.rmtree(outside, ignore_errors=True)
                reset()
        # Review of 0287e858e4: the test above stops at the lstat. A
        # retention directory replaced by a link after the lstat found a
        # directory, before the open, is refused by that open itself.
        with self.subTest("retention directory replaced by a link after it was found to be a directory"):
            outside = self.root / "outside-swapped-retention"
            moved = writer.parent / "moved-swapped-retention"
            real_lstat = os.lstat
            retained = Path()
            swapped: list[bool] = []

            def swapping(name, *rest, dir_fd=None, **options):
                found = real_lstat(name, *rest, dir_fd=dir_fd, **options)
                if dir_fd is not None and name == retained.name and not swapped:
                    swapped.append(True)
                    shutil.copytree(retained, outside, symlinks=True)
                    os.rename(retained, moved)
                    os.symlink(outside, retained)
                return found

            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                with mock.patch.object(mirror.os, "lstat", swapping):
                    self.assertEqual("", prune(later()))
                self.assertEqual([True], swapped)
                self.assertEqual(stored, (outside / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)],
                                 json.loads((outside / "displaced.json").read_text())["files"])
                self.assertEqual(stored, (moved / "files" / scorecard).read_bytes())
            finally:
                if retained.is_symlink():
                    retained.unlink()
                shutil.rmtree(moved, ignore_errors=True)
                shutil.rmtree(outside, ignore_errors=True)
                reset()
        # Review of 36da12785d: a files directory, or a directory under it,
        # replaced by a symbolic link was followed, so the prune deleted the
        # bytes the link led to.
        with self.subTest("files directory or a directory under it replaced by a link"):
            outside = self.root / "outside-files"
            try:
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [retained] = displaced()
                shutil.copytree(retained / "files", outside)
                shutil.rmtree(retained / "files")
                os.symlink(outside, retained / "files")
                # A link where a directory is opened with O_NOFOLLOW fails
                # with ENOTDIR (NotADirectoryError) on Linux, ELOOP elsewhere.
                self.assertRegex(prune(later()), re.escape(
                    f"could not finish emptying the files an earlier publication moved to {retained}/files (")
                    + r"(NotADirectoryError|OSError): ")
                self.assertEqual(stored, (outside / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
                (retained / "files").unlink()
                (retained / "files").mkdir()
                os.symlink(outside / "scorecard", retained / "files" / "scorecard")
                self.assertIn(f"KEPT 1 file(s) an earlier publication moved to {retained}/files: {scorecard} is "
                              "not reachable inside its retention directory through directories alone, without "
                              "following a symbolic link (", prune(later()))
                self.assertEqual(stored, (outside / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(retained))
            finally:
                shutil.rmtree(outside, ignore_errors=True)
                reset()
        # Review of 36da12785d: a record float() or int() could not convert
        # (1e999, a 401-digit time) or json could not parse (deep nesting)
        # raised out of the prune and stopped the publication, and so did a
        # files link to itself. Neither may: a record that cannot be used is
        # left alone, and any other error keeps the files it was about.
        with self.subTest("record prune cannot use, and an error prune does not expect"):
            digest = hashlib.sha256(stored).hexdigest()
            item = '{"path": "%s", "sha256": "%s", "size": %%s}' % (scorecard, digest)
            records = {
                "infinite-size": '{"displaced_at": 0, "files": [%s]}' % (item % "1e999"),
                "huge-time": '{"displaced_at": 1%s, "files": [%s]}' % ("0" * 400, item % len(stored)),
                "text-size": '{"displaced_at": 0, "files": [%s]}' % (item % '"%d"' % len(stored)),
                "boolean-size": '{"displaced_at": 0, "files": [%s]}' % (item % "true"),
                "nested": "[" * 100000,
            }
            made = []
            for label, text in records.items():
                retained = writer.parent / f"{writer.name}.displaced-{label}"
                (retained / "files" / scorecard).parent.mkdir(parents=True)
                (retained / "files" / scorecard).write_bytes(stored)
                (retained / "displaced.json").write_text(text)
                made.append(retained)
            looped = writer.parent / f"{writer.name}.displaced-looped"
            looped.mkdir()
            (looped / "displaced.json").write_text(json.dumps({"displaced_at": 0, "files": [entry(scorecard, stored)]}))
            os.symlink("files", looped / "files")
            try:
                pruned = prune(later())
                self.assertRegex(pruned, re.escape(
                    f"could not finish emptying the files an earlier publication moved to {looped}/files (")
                    + r"(NotADirectoryError|OSError): ")
                for retained in made:
                    self.assertNotIn(f"{retained}/files", pruned)
                    self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
                    self.assertTrue((retained / "displaced.json").exists())
                mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                [young] = set(displaced()) - {*made, looped}
                with mock.patch.object(mirror, "_empty_if_unchanged", create=True,
                                       side_effect=RuntimeError("unexpected")):
                    pruned = prune(later())
                self.assertIn(f"could not finish emptying the files an earlier publication moved to {young}/files "
                              "(RuntimeError: unexpected); they are kept", pruned)
                self.assertEqual(stored, (young / "files" / scorecard).read_bytes())
                self.assertEqual([entry(scorecard, stored)], recorded(young))
                git(writer, "checkout", "-q", "-f", "--detach", "HEAD")
                # A publication goes ahead past all of them.
                render("unusable records")
                self.append("unusable-records")
                rows.append(row("unusable-records"))
                report = self.publish()
                self.assertEqual("published", report["state"], report)
                self.assertEqual(rows, self.remote_rows())
                for retained in made:
                    self.assertEqual(stored, (retained / "files" / scorecard).read_bytes())
            finally:
                reset()
        # Review of 36da12785d: GIT_CONFIG names the one file `git config`
        # reads, and no other Git command reads it, so the census missed a
        # driver the comparison then ran.
        with self.subTest("filter driver hidden from the census by an inherited GIT_CONFIG"):
            edit, smudged = self.root / "hidden-driver-edit", self.root / "smudged-hidden"
            edit.write_bytes(b"STRANDED\n")
            git(writer, "config", "filter.hidden.smudge",
                f"touch {shlex.quote(str(smudged))} && cat {shlex.quote(str(edit))}")
            info.mkdir(parents=True, exist_ok=True)
            attributes.write_bytes(f"/{scorecard} filter=hidden\n".encode())
            try:
                (writer / scorecard).write_bytes(b"STRANDED\n")
                with mock.patch.dict(os.environ, {"GIT_CONFIG": os.devnull}):
                    self.assertTrue(any(scorecard in line for line in mirror._unstored_bytes(writer)))
                    with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(
                            f"ledger writer path {scorecard} held bytes its HEAD does not store")):
                        mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                self.assertFalse(smudged.exists(), "a filter program ran during the replacement")
                [retained] = displaced()
                self.assertEqual(b"STRANDED\n", (retained / "files" / scorecard).read_bytes())
                self.assertEqual([], recorded(retained))
            finally:
                subprocess.run(["git", "-C", str(writer), "config", "--unset", "filter.hidden.smudge"],
                               capture_output=True, check=False)
                if saved_attributes is None:
                    attributes.unlink(missing_ok=True)
                else:
                    attributes.write_bytes(saved_attributes)
                reset()
        # Review of 6f4e0db6e1: the driver with an empty name ([filter ""],
        # selected by `filter=`) was left running, so a smudge program that
        # prints the edit made it compare as stored at the replacement too.
        with self.subTest("edit a smudge filter driver with an empty name would print"):
            edit, smudged = self.root / "empty-driver-edit", self.root / "smudged-at-replacement"
            edit.write_bytes(b"STRANDED\n")
            git(writer, "config", "filter..smudge",
                f"touch {shlex.quote(str(smudged))} && cat {shlex.quote(str(edit))}")
            info.mkdir(parents=True, exist_ok=True)
            attributes.write_bytes(f"/{scorecard} filter=\n".encode())
            try:
                (writer / scorecard).write_bytes(b"STRANDED\n")
                with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(
                        f"ledger writer path {scorecard} held bytes its HEAD does not store")):
                    mirror._replace_writer_files(writer, {scorecard: b"replacement\n"})
                self.assertFalse(smudged.exists(), "a filter program ran during the replacement")
                [retained] = displaced()
                self.assertEqual(b"STRANDED\n", (retained / "files" / scorecard).read_bytes())
                self.assertEqual([], recorded(retained))
            finally:
                subprocess.run(["git", "-C", str(writer), "config", "--unset", "filter..smudge"],
                               capture_output=True, check=False)
                if saved_attributes is None:
                    attributes.unlink(missing_ok=True)
                else:
                    attributes.write_bytes(saved_attributes)
                reset()
        # Review of 6f4e0db6e1: a reader holding a file open refused the
        # whole publication. It now publishes; the file it reads is kept
        # until it is closed.
        with self.subTest("file a reader has open while a publication replaces it"):
            render("reader")
            self.append("reader")
            rows.append(row("reader"))
            # What HEAD stores now; an earlier subtest published past `stored`.
            held = (writer / scorecard).read_bytes()
            reader = os.open(writer / scorecard, os.O_RDONLY)
            try:
                report = self.publish()
                self.assertEqual("published", report["state"], report)
                self.assertEqual(b"rendered for reader\n", (writer / scorecard).read_bytes())
                self.assertEqual(rows, self.remote_rows())
                [retained] = holding(scorecard)
                self.assertIn(f"{scorecard} is open in another process", prune(later()))
                self.assertEqual([entry(scorecard, held)], recorded(retained))
                # The publication's other replacements, unchanged and closed,
                # were emptied and removed with their records and
                # directories; only the directory holding the file the reader
                # has open is left, its record still naming that file.
                self.assertEqual([retained], displaced())
            finally:
                os.close(reader)
            prune(later())
            emptied(retained, [scorecard])
        # A clean publication replaces only stored bytes and records each one;
        # the next one, once they are old enough, empties them.
        reset()
        render("clean")
        self.append("clean")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        kept = displaced()
        self.assertNotEqual([], kept)
        names = {}
        for retained in kept:
            for item in recorded(retained):
                self.assertEqual(entry(item["path"], (retained / "files" / item["path"]).read_bytes()), item)
            names[retained] = [item["path"] for item in recorded(retained)]
        self.assertEqual(b"rendered for clean\n", (writer / scorecard).read_bytes())
        self.assertEqual(b'{"rendered": "clean"}\n', stored_history(self.remote))
        self.assertEqual([*rows, row("clean")], self.remote_rows())
        render("next")
        self.append("next")
        with mock.patch.object(mirror, "DISPLACED_RETENTION_SECONDS", 0):
            report = self.publish()
        self.assertEqual("published", report["state"], report)
        for retained, paths in names.items():
            emptied(retained, paths)
        self.assertEqual([*rows, row("clean"), row("next")], self.remote_rows())

    def test_a_publication_refuses_a_sparse_writer_before_it_changes_anything(self):
        # Review of 1335ba29ec: under a sparse checkout the regeneration wrote
        # files outside the patterns, which `git add` refused, leaving staged
        # metadata and written files that blocked every later run. The
        # publication refuses when it starts instead.
        self.append("published-first")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        head = git(writer, "rev-parse", "HEAD")
        git(writer, "sparse-checkout", "set", "series")
        try:
            self.assertFalse((writer / "scorecard").exists())
            self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
            self.append("pending")
            with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(
                    "ledger writer has a sparse checkout, which a publication cannot write its "
                    "scorecard into; nothing was published")):
                self.publish()
            self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
            self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
            self.assertEqual("", git(writer, "diff", "--cached", "--name-only"))
            self.assertFalse((writer / "scorecard").exists())
            self.assertEqual([row("published-first")], self.remote_rows())
        finally:
            git(writer, "sparse-checkout", "disable")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("published-first"), row("pending")], self.remote_rows())

    def test_a_publication_clears_index_bits_that_git_add_does_not_honour(self):
        # Review of 1335ba29ec, with a probe of git 2.53: `git add` refuses a
        # path under a skip-worktree bit when named and skips it under its
        # directory, so a deleted stale history file stayed stored beside the
        # new parts; under an assume-unchanged bit it stages nothing, so the
        # old bytes were published and the regenerated ones left on disk.
        # Neither bit shows in git status. Review of 8f2b7800de: refusing the
        # bits stalled a writer under core.ignoreStat, where Git sets them
        # itself. The publication clears them once its stored-byte check has
        # passed, and publishes what it renders.
        self.append("published-first")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        marked = "scorecard/SCORECARD.md"
        rows = [row("published-first")]
        for index, flag in enumerate(("--skip-worktree", "--assume-unchanged")):
            with self.subTest(flag):
                rendered = f"rendered under {flag}\n"
                self.set_tool(f"(p/'SCORECARD.md').write_text({rendered!r})\n")
                git(writer, "update-index", flag, marked)
                self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                self.append(f"pending-{index}")
                rows.append(row(f"pending-{index}"))
                report = self.publish()
                self.assertEqual("published", report["state"], report)
                self.assertEqual(rendered, git(self.remote, "show", f"main:{marked}") + "\n")
                self.assertEqual(git(self.remote, "rev-parse", "main"), git(writer, "rev-parse", "HEAD"))
                self.assertEqual(f"H {marked}", git(writer, "ls-files", "-v", "--", marked))
                self.assertEqual(rendered.encode(), (writer / marked).read_bytes())
                self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                self.assertEqual(rows, self.remote_rows())
        # A bit can also hide a change that holds no new bytes, such as a mode
        # change: the stored-byte check passes, and git status shows it only
        # once the bit is cleared. No commit stores it, so the publication
        # refuses there, before it publishes anything.
        with self.subTest("mode change under an assume-unchanged bit"):
            head = git(writer, "rev-parse", "HEAD")
            git(writer, "update-index", "--assume-unchanged", marked)
            (writer / marked).chmod(0o755)
            try:
                self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))
                self.assertEqual([], mirror._unstored_bytes(writer))
                self.append("pending-mode")
                with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(
                        "ledger writer index bits were cleared but the writer is not clean (still marked: "
                        f"none; changed: {marked}); nothing more was written")):
                    self.publish()
                self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(git(self.remote, "rev-parse", "main"), head)
                self.assertEqual(rows, self.remote_rows())
                self.assertEqual(f"H {marked}", git(writer, "ls-files", "-v", "--", marked))
                self.assertEqual(f"M {marked}", git(writer, "status", "--porcelain", "--untracked-files=all"))
                # Review of 6f4e0db6e1: with the bit cleared, the next
                # publication's cleanup saw a modified scorecard file and
                # rescued the operator's mode change as render output. It is
                # refused again, with no recovery ref and the mode kept.
                refs = git(writer, "for-each-ref", "refs/heads/recovery/")
                with self.assertRaisesRegex(mirror.MirrorRefused, re.escape(
                        "ledger writer has unpublished work")), contextlib.redirect_stderr(io.StringIO()):
                    self.publish()
                self.assertEqual(refs, git(writer, "for-each-ref", "refs/heads/recovery/"))
                self.assertEqual(head, git(writer, "rev-parse", "HEAD"))
                self.assertEqual(rows, self.remote_rows())
                self.assertEqual(0o755, (writer / marked).stat().st_mode & 0o777)
                self.assertEqual(f"M {marked}", git(writer, "status", "--porcelain", "--untracked-files=all"))
            finally:
                (writer / marked).chmod(0o644)
            rows.append(row("pending-mode"))
            report = self.publish()
            self.assertEqual("published", report["state"], report)
            self.assertEqual(rows, self.remote_rows())
            self.assertEqual("", git(writer, "status", "--porcelain", "--untracked-files=all"))

    def test_consecutive_publications_under_core_ignorestat_stage_what_they_write(self):
        # Review of 8f2b7800de: under core.ignoreStat Git sets an
        # assume-unchanged bit on every entry a checkout, add or refresh
        # writes, so one publication's own commands marked what the next had
        # to stage. Three publications in a row, each after another host
        # moved main, must each publish what they render.
        config = Path(os.environ["GIT_CONFIG_GLOBAL"])
        saved = config.read_bytes() if config.exists() else None

        def restore() -> None:
            if saved is None:
                config.unlink(missing_ok=True)
            else:
                config.write_bytes(saved)

        self.addCleanup(restore)
        config.write_bytes((saved or b"") + b"[core]\n\tignoreStat = true\n")
        self.append("published-first")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        # The other host does not run under the setting, or its own `git add`
        # would skip what it edits.
        other = self.root / "other-host-ignorestat"
        subprocess.run(["git", "-c", "core.ignoreStat=false", "clone", "-q", str(self.remote), str(other)],
                       check=True)

        def on_other(*args: str) -> None:
            git(other, "-c", "core.ignoreStat=false", *args)

        marked, notes = "scorecard/SCORECARD.md", "notes/README"
        rows = [row("published-first")]
        for index in range(3):
            with self.subTest(round=index):
                on_other("pull", "-q", "--ff-only", "origin", "main")
                (other / marked).write_text(f"another host, round {index}\n")
                on_other("add", marked)
                if index == 0:
                    (other / notes).parent.mkdir(parents=True, exist_ok=True)
                    (other / notes).write_text("kept by another host\n")
                    on_other("add", notes)
                on_other("commit", "-qm", f"another host edited the scorecard, round {index}")
                self.assertEqual(f"another host, round {index}", git(other, "show", f"HEAD:{marked}"))
                on_other("push", "-q", "origin", "main")
                if index == 1:
                    # A stored file a skip-worktree bit leaves absent is
                    # written back; under core.ignoreStat that write is
                    # checked with the same override.
                    git(writer, "update-index", "--skip-worktree", notes)
                    (writer / notes).unlink()
                rendered = f"rendered in round {index}\n"
                # The fixture's own `git add` of the renderer is under the
                # setting too: clear the bit its last add set.
                git(self.tool, "-c", "core.ignoreStat=false", "update-index", "--no-assume-unchanged",
                    "ci/compat-envelope/scorecard.rs")
                self.set_tool(f"(p/'SCORECARD.md').write_text({rendered!r})\n")
                self.append(f"pending-{index}")
                rows.append(row(f"pending-{index}"))
                report = self.publish()
                self.assertEqual("published", report["state"], report)
                self.assertEqual(rendered, git(self.remote, "show", f"main:{marked}") + "\n")
                self.assertEqual("kept by another host", git(self.remote, "show", f"main:{notes}"))
                self.assertEqual(git(self.remote, "rev-parse", "main"), git(writer, "rev-parse", "HEAD"))
                self.assertEqual(b"kept by another host\n", (writer / notes).read_bytes())
                self.assertEqual("", git(writer, "-c", "core.ignoreStat=false", "status", "--porcelain",
                                         "--untracked-files=all"))
                self.assertEqual([], mirror._unstored_bytes(writer))
                self.assertEqual(rows, self.remote_rows())
        # Another host can push between the writer's fetch and the
        # publisher's. The publisher's own checkout of that newer main writes
        # the other host's scorecard under the setting, which marks it, and
        # the regenerated scorecard must still be staged (_copy_back clears
        # the bit again).
        real_git = mirror.publisher._git
        tags: list[str] = []
        moved: list[str] = []

        def racing(repo, *args, **kwargs):
            if args[:1] != ("checkout",) or moved:
                return real_git(repo, *args, **kwargs)
            on_other("pull", "-q", "--ff-only", "origin", "main")
            (other / marked).write_text("another host, during the publication\n")
            on_other("commit", "-qam", "another host edited the scorecard during the publication")
            on_other("push", "-q", "origin", "main")
            moved.append(git(other, "rev-parse", "HEAD"))
            git(writer, "-c", "core.ignoreStat=false", "fetch", "-q", "origin", "main")
            result = real_git(repo, *args, **kwargs)
            tags.append(git(writer, "ls-files", "-v", "--", marked))
            return result

        with self.subTest(round="push during the publication"):
            rendered = "rendered during the race\n"
            git(self.tool, "-c", "core.ignoreStat=false", "update-index", "--no-assume-unchanged",
                "ci/compat-envelope/scorecard.rs")
            self.set_tool(f"(p/'SCORECARD.md').write_text({rendered!r})\n")
            self.append("pending-race")
            rows.append(row("pending-race"))
            with mock.patch.object(mirror.publisher, "_git", racing):
                report = self.publish()
            # The publisher's checkout of the newer main marked the scorecard.
            self.assertEqual([f"h {marked}"], tags)
            self.assertEqual("published", report["state"], report)
            git(self.remote, "merge-base", "--is-ancestor", *moved, "main")
            self.assertEqual(rendered, git(self.remote, "show", f"main:{marked}") + "\n")
            self.assertEqual(git(self.remote, "rev-parse", "main"), git(writer, "rev-parse", "HEAD"))
            self.assertEqual(rendered.encode(), (writer / marked).read_bytes())
            self.assertEqual("", git(writer, "-c", "core.ignoreStat=false", "status", "--porcelain",
                                     "--untracked-files=all"))
            self.assertEqual([], mirror._unstored_bytes(writer))
            self.assertEqual(rows, self.remote_rows())

    def test_a_writer_that_writes_symbolic_links_as_files_still_publishes(self):
        # Review of 1335ba29ec: under core.symlinks=false a checkout writes a
        # stored symbolic link as a regular file holding its target. That
        # file holds no bytes HEAD does not store; refusing it stalled every
        # publication from such a writer.
        self.append("published-first")
        self.assertEqual("published", self.publish()["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        git(writer, "config", "core.symlinks", "false")
        other = self.root / "other-host-link"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        link = "scorecard/CURRENT.md"
        os.symlink("SCORECARD.md", other / link)
        git(other, "add", link)
        git(other, "commit", "-qm", "another host linked the scorecard")
        git(other, "push", "-q", "origin", "main")
        self.append("pending")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertFalse((writer / link).is_symlink())
        self.assertEqual(b"SCORECARD.md", (writer / link).read_bytes())
        self.assertEqual("120000", git(writer, "ls-tree", "HEAD", "--", link).split()[0])
        self.assertEqual([], mirror._unstored_bytes(writer))
        # A file there holding anything else is still refused.
        (writer / link).write_bytes(b"SCORECARD.md, edited")
        self.assertEqual([f"{link!r} differs from the bytes HEAD stores"], mirror._unstored_bytes(writer))
        git(writer, "checkout", "--", link)
        self.append("pending-again")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("published-first"), row("pending"), row("pending-again")], self.remote_rows())

    def test_a_writer_under_submodule_recurse_leaves_a_gitlink_unpopulated(self):
        # Review of 1335ba29ec: under submodule.recurse every writer checkout
        # recursed, so a gitlink main added back was populated from the module
        # directory the writer kept, and every later publication refused that
        # checkout. Every writer checkout passes --no-recurse-submodules.
        self.append("published-first")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        writer = self.parent / mirror.WRITER_RELATIVE
        inner = self.root / "inner"
        subprocess.run(["git", "init", "-q", str(inner)], check=True)
        (inner / "notes.txt").write_text("vendored\n")
        git(inner, "add", "notes.txt")
        git(inner, "commit", "-qm", "inner")
        other = self.root / "other-host-module"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True)
        git(other, "-c", "protocol.file.allow=always", "submodule", "add", "-q", str(inner), "vendored")
        git(other, "commit", "-qm", "another host vendored a module")
        git(other, "push", "-q", "origin", "main")
        # The writer populates it once, then follows main's removal of it,
        # keeping the module directory.
        git(writer, "fetch", "-q", "origin")
        git(writer, "checkout", "-q", "--detach", "refs/remotes/origin/main")
        git(writer, "-c", "protocol.file.allow=always", "submodule", "update", "-q", "--init")
        self.assertIn("notes.txt", os.listdir(writer / "vendored"))
        git(writer, "config", "submodule.recurse", "true")
        git(other, "rm", "-q", "vendored")
        git(other, "commit", "-qm", "another host removed the module")
        git(other, "push", "-q", "origin", "main")
        git(writer, "fetch", "-q", "origin")
        git(writer, "checkout", "-q", "--detach", "refs/remotes/origin/main")
        self.assertFalse((writer / "vendored").exists())
        git(other, "revert", "--no-edit", "HEAD")
        git(other, "push", "-q", "origin", "main")
        self.append("pending")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual("160000", git(writer, "ls-tree", "HEAD", "--", "vendored").split()[0])
        self.assertEqual([], os.listdir(writer / "vendored"))
        self.assertEqual([], mirror._unstored_bytes(writer))
        self.append("pending-again")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("published-first"), row("pending"), row("pending-again")], self.remote_rows())
        # Review of 8f2b7800de: the reset that sets a render leftover aside
        # honoured submodule.recurse and populated the gitlink, which every
        # later publication then refused. (On record as a render's output,
        # as _replace_writer_files leaves it.)
        mirror._record_render_provenance(writer, [("scorecard/SCORECARD.md", b"killed render\n")])
        (writer / "scorecard/SCORECARD.md").write_text("killed render\n")
        self.append("after-leftover")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertRegex(" ".join(report["writer_cleaned"]), "saved as refs/heads/recovery/render-")
        self.assertEqual([], os.listdir(writer / "vendored"))
        self.assertEqual([], mirror._unstored_bytes(writer))
        self.append("after-cleanup")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("published-first"), row("pending"), row("pending-again"), row("after-leftover"),
                          row("after-cleanup")], self.remote_rows())

    def test_renderer_gets_a_ledger_copy_without_routes_credentials_or_the_wrapper(self):
        # F4: the renderer is measured code. It sees a throwaway ledger copy
        # (its origin URL intact for scorecard.rs, its push URL dead), the real
        # publication lock, no proxy or credential variables, none of the Git
        # configuration that carries the host's proxy and credential helper,
        # and is not run through the network wrapper.
        writer = self.parent / mirror.WRITER_RELATIVE
        dump = self.root / "renderer-view.json"
        self.set_tool(
            "import json\n"
            "def config(*args):\n"
            " return subprocess.run(['git','-C',str(ledger),'config',*args],"
            "capture_output=True,text=True).stdout.strip()\n"
            "lock=ledger/'.git'/'ci-hub-series-publication.lock'\n"
            "json.dump({'env': dict(os.environ), 'ledger': str(ledger), 'url': config('--get','remote.origin.url'),"
            " 'pushurl': config('--get','remote.origin.pushurl'),"
            " 'config': config('--list'), 'lock': os.path.realpath(lock),"
            " 'same_lock': os.path.samestat(os.fstat(fd), os.stat(lock))}, open("
            + repr(str(dump)) + ", 'w'))\n"
            "(p/'SCORECARD.md').write_text('rendered in a copy\\n')\n")
        # The publisher's own Git configuration names a proxy and a credential
        # helper, in its global file and through the environment. The fixture's
        # remote mapping stays, so the publisher's own fetch and push still work.
        global_config = self.root / "publisher-gitconfig"
        global_config.write_text((self.root / "gitconfig").read_text()
                                 + "[http]\n\tproxy = http://global.must-not-reach.invalid:1\n"
                                 "\tproxysslcert = /must-not-reach/x509.pem\n"
                                 "\tproxysslkey = /must-not-reach/x509.pem\n"
                                 "[credential]\n\thelper = !echo must-not-reach-the-renderer\n")
        system_config = self.root / "publisher-system-gitconfig"
        system_config.write_text("[http]\n\tproxy = http://system.must-not-reach.invalid:1\n")
        ambient = {name: "must-not-reach-the-renderer" for name in mirror.RENDERER_WITHHELD_ENV}
        ambient.update({
            "GIT_CONFIG_GLOBAL": str(global_config),
            "GIT_CONFIG_SYSTEM": str(system_config), "GIT_CONFIG_NOSYSTEM": "0",
            "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "http.proxy",
            "GIT_CONFIG_VALUE_0": "http://count.must-not-reach.invalid:1",
            "GIT_CONFIG_PARAMETERS": "'credential.helper'='!echo parameters-must-not-reach'",
        })
        log = self.root / "wrapper.log"
        wrapper = self.root / "bin/logging-wrapper"
        wrapper.write_text("#!/usr/bin/python3\nimport json, os, sys\n"
                           f"open({str(log)!r}, 'a').write(json.dumps(sys.argv[1:]) + '\\n')\n"
                           "os.execvp(sys.argv[1], sys.argv[1:])\n")
        wrapper.chmod(0o755)
        self.append("isolated-render")
        with mock.patch.dict(os.environ, ambient):
            report = mirror.mirror(self.parent, self.source, self.target, mirror.REMOTE_URL,
                                   net_wrapper=str(wrapper), tool_checkout=self.tool)
        self.assertEqual("published", report["state"], report)
        seen = json.loads(dump.read_text())
        self.assertEqual([], sorted(set(mirror.RENDERER_WITHHELD_ENV) & set(seen["env"])))
        self.assertEqual("true", seen["env"]["CARGO_NET_OFFLINE"])
        self.assertEqual("0", seen["env"]["GIT_TERMINAL_PROMPT"])
        # Git on the fleet hosts may itself add request headers on its command
        # line; no key may name a proxy or its client certificate (http.proxy,
        # http.proxysslcert, http.proxysslkey, as ~/.gitconfig on devbig014
        # sets), a client key or certificate, or a credential helper, and
        # nothing the publisher configured may appear.
        config_keys = [line.split("=", 1)[0].lower() for line in seen["config"].splitlines()]
        self.assertEqual([], [key for key in config_keys if key.startswith("credential.")
                              or key.rsplit(".", 1)[-1].startswith("proxy")
                              or key.rsplit(".", 1)[-1] in ("sslkey", "sslcert")], seen["config"])
        self.assertNotIn("must-not-reach", seen["config"])
        self.assertEqual(os.devnull, seen["env"]["GIT_CONFIG_GLOBAL"])
        self.assertEqual("1", seen["env"]["GIT_CONFIG_NOSYSTEM"])
        ledger = Path(seen["ledger"])
        self.assertNotEqual(writer.resolve(), ledger.resolve())
        self.assertTrue(ledger.parent.name.startswith(mirror.RENDER_COPY_PREFIX), ledger)
        self.assertEqual(writer.parent.resolve(), ledger.parent.parent.resolve())
        self.assertEqual(mirror.REMOTE_URL, seen["url"])
        self.assertTrue(seen["pushurl"].startswith(str(ledger)), seen["pushurl"])
        self.assertTrue(seen["same_lock"])
        common = Path(git(writer, "rev-parse", "--path-format=absolute", "--git-common-dir"))
        self.assertEqual(str((common / mirror.PUBLICATION_LOCK).resolve()), seen["lock"])
        self.assertFalse(ledger.exists(), "the copy is removed after the render")
        self.assertEqual([], list(writer.parent.glob(mirror.RENDER_COPY_PREFIX + "*")))
        invocations = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertTrue(any("push" in argv for argv in invocations), invocations)
        tool = str(self.tool / "ci/compat-envelope/scorecard.rs")
        self.assertTrue(any(tool in argv for argv in invocations), "prepare_scorecard uses the wrapper")
        self.assertEqual([], [argv for argv in invocations if tool in argv and argv[-1] != "--help"])
        self.assertEqual("rendered in a copy\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertEqual("", git(writer, "status", "--porcelain"))

    def test_stale_render_copy_from_a_killed_publisher_is_removed(self):
        writer = self.parent / mirror.WRITER_RELATIVE
        self.append("before-stale")
        self.assertEqual("published", self.publish()["state"])
        stale = writer.parent / (mirror.RENDER_COPY_PREFIX + "killed")
        (stale / "hermit_test_ledger").mkdir(parents=True)
        outside = writer.parent / "unrelated-render-state"
        outside.mkdir()
        self.append("after-stale")
        self.assertEqual("published", self.publish()["state"])
        self.assertFalse(stale.exists())
        self.assertTrue(outside.exists())

    def test_shallow_measured_checkout_still_publishes_one_derived_carrier(self):
        # F2: a head order that cannot be decided withholds the scorecard, not
        # the proof. The published head is the root; the measured checkout is
        # a depth-1 clone of a descendant, so ancestry cannot be decided.
        self.use_head_renderer()
        self.append("scored-root")
        self.assertEqual("published", self.publish_at(self.target)[0]["scorecard"]["state"])
        before = self.published_projection()
        child = self.source_commit("descendant", "2030-01-01T00:00:00Z")
        shallow = self.root / "shallow-hermit"
        subprocess.run(["git", "clone", "-q", "--depth", "1", f"file://{self.source}", str(shallow)], check=True)
        self.assertEqual(child, git(shallow, "rev-parse", "HEAD"))
        self.source = shallow
        self.target = child
        prepared, encoded = self.derived_fixture()
        self.append("shallow-run")
        with self.proof_doubles(prepared, encoded), \
                mock.patch.object(mirror, "build_scorecard", wraps=mirror.build_scorecard) as renderer:
            report, stderr = self.publish_at(child, results=self.root / "results",
                                             finalized_run_id=prepared["selection"]["row"]["run_id"])
        renderer.assert_not_called()
        self.assertEqual("derived-failed", report["state"], report)
        self.assertEqual("failed", report["scorecard"]["state"])
        self.assertIn("scorecard head order cannot be decided: measured checkout is shallow",
                      report["scorecard"]["error"])
        self.assertEqual("published", report["portable_proof"]["state"], report)
        self.assertEqual(report["commit"], report["portable_proof"]["commit"])
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        self.assertEqual(1, len(self.carrier_paths()))
        self.assertEqual(before, self.published_projection())
        self.assertIn(row("shallow-run"), self.remote_rows())
        self.assertIn("SCORECARD FAILED", stderr)
        self.assertIn("finalized proof published", stderr)

    def test_unordered_published_head_still_publishes_one_derived_carrier(self):
        # F2: latest.json names a head this checkout cannot read and records
        # no commit time (the pre-ordering format), so no order exists.
        clone = self.root / "seed-latest"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(clone)], check=True)
        (clone / "latest.json").write_text(json.dumps(
            {"schema": mirror.METADATA_SCHEMA, "scorecard_hermit_commit": "e" * 40}, indent=2) + "\n")
        git(clone, "add", "latest.json")
        git(clone, "commit", "-qm", "fixture: an unreadable head with no recorded time")
        git(clone, "push", "-q", "origin", "HEAD:main")
        before = self.remote_file("latest.json")
        prepared, encoded = self.derived_fixture()
        self.append("unordered-run")
        with self.proof_doubles(prepared, encoded), \
                mock.patch.object(mirror, "build_scorecard", wraps=mirror.build_scorecard) as renderer:
            report, stderr = self.publish_at(self.target, results=self.root / "results",
                                             finalized_run_id=prepared["selection"]["row"]["run_id"])
        renderer.assert_not_called()
        self.assertEqual("derived-failed", report["state"], report)
        self.assertIn("scorecard head order cannot be decided", report["scorecard"]["error"])
        self.assertIn(f"records no {mirror.SCORECARD_HEAD_TIME_FIELD}", report["scorecard"]["error"])
        self.assertEqual("published", report["portable_proof"]["state"], report)
        self.assertEqual(report["commit"], git(self.remote, "rev-parse", "main"))
        self.assertEqual(1, len(self.carrier_paths()))
        self.assertEqual(before, self.remote_file("latest.json"))
        self.assertEqual("legacy scorecard\n", self.remote_file("scorecard/SCORECARD.md"))
        self.assertIn("SCORECARD FAILED", stderr)


    # ------------------------------------------------ parity rides with series
    # https://github.com/rrnewton/hermit/issues/3301

    PARITY_RUN = "validate-coord-s17-d3a0a4ae3d16-1790649970094317174-3328074-da07a1da"
    PARITY_TREE = "d3a0a4ae3d168565595ae4157b25ab8efbb1861a"

    def parity_sources(self) -> list[dict]:
        fixture = Path(__file__).resolve().parents[2] / "tests/fixtures/parity-run-1953"
        parity = series.parity_ledger_module()
        text = (fixture / "ledger-sources-typed.jsonl").read_text(encoding="utf-8")
        return [parity.loads(line, "fixture") for line in text.splitlines()]

    def spool_parity(self, events: list[dict] | None = None) -> Path:
        """Spool one run's batch as `series.py append-parity` would."""
        parity = series.parity_ledger_module()
        if events is None:
            events = parity.build_batch(
                self.parity_sources(), producer="validate", run_id=self.PARITY_RUN,
                hermit_sha=self.PARITY_TREE, emitted_at="2026-09-21T12:00:00Z",
                source_tree_dirty=False, host="fixture")
        spool = series.parity_spool_dir(self.parent)
        spool.mkdir(parents=True, exist_ok=True)
        path = spool / parity.batch_name(events[0])
        path.write_text(parity.encode_batch(events), encoding="utf-8")
        return path

    def spool_torn_refusal(self, run_id: str) -> Path:
        """Spool another run's refusal batch, torn before its final newline."""
        parity = series.parity_ledger_module()
        refusal = parity.build_refusal(
            producer="validate", run_id=run_id, hermit_sha=self.PARITY_TREE,
            emitted_at="2026-09-21T12:00:00Z", source_tree_dirty=False, host="fixture",
            input_bytes=b"{}\n", input_lines=1, error="input row 1: fixture refusal")
        spool = series.parity_spool_dir(self.parent)
        spool.mkdir(parents=True, exist_ok=True)
        path = spool / parity.batch_name(refusal)
        path.write_text(parity.encode_batch([refusal])[:-1], encoding="utf-8")
        return path

    def commit_parity(self, writer: Path, events: list[dict]) -> str:
        """Commit EVENTS to the writer's parity shards as the publisher does,
        and push nothing: what a crash between commit and push leaves."""
        parity = series.parity_ledger_module()
        shards: list[str] = []
        for event in events:
            relative = parity.event_shard(event)
            shard = writer / relative
            shard.parent.mkdir(parents=True, exist_ok=True)
            with shard.open("a", encoding="utf-8") as handle:
                handle.write(parity.canonical_line(event) + "\n")
            if relative not in shards:
                shards.append(relative)
        git(writer, "add", "--", *shards)
        git(writer, "commit", "-qm", f"parity: append {len(events)} event(s)")
        return git(writer, "rev-parse", "HEAD")

    def run_registry(self):
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub"))
        import run_registry
        return run_registry

    def test_parity_publishes_in_the_series_commit_and_the_run_is_located(self):
        self.append("run-parity")
        batch = self.spool_parity()
        report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        parity = report["raw_publication"]["parity"]
        head = git(self.remote, "rev-parse", "main")
        self.assertEqual({
            "schema": "parity-publication/v1", "published": 193, "already_published": 0,
            "commit": head, "joint": True, "batches": [batch.name], "refused": [],
            "error": None, "pending_batches": 0, "historic_invalid": 0,
            "run": {"producer": "validate", "run_id": self.PARITY_RUN,
                    "state": "published", "records": 192, "refusal": None},
        }, parity)
        self.assertEqual("series: append 1 event(s); parity: append 193 event(s)",
                         git(self.remote, "log", "-1", "--format=%s", "main"))
        self.assertEqual([row("run-parity")], self.remote_rows())
        self.assertEqual(192, len(self.remote_file("parity/hermit/fixture/2026-09.jsonl").splitlines()))
        self.run_registry().check_parity_publication(parity)
        # A second publication finds nothing new and still locates the run.
        again = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("unchanged", again["state"], again)
        self.assertEqual(0, again["raw_publication"]["parity"]["published"])
        self.assertEqual({"producer": "validate", "run_id": self.PARITY_RUN,
                          "state": "published", "records": 192, "refusal": None},
                         again["raw_publication"]["parity"]["run"])
        # No marker and no refusal: append-parity was never called for the
        # run (an older hermit, or no parity cell in scope; not told apart).
        other = self.publish(publish_only=True, finalized_run_id="validate-older-hermit")
        self.assertEqual({"producer": "validate", "run_id": "validate-older-hermit",
                          "state": "absent", "records": None, "refusal": None},
                         other["raw_publication"]["parity"]["run"])
        self.run_registry().check_parity_publication(other["raw_publication"]["parity"])

    def test_the_publisher_is_entered_with_both_spool_locks_free(self):
        # A producer waits at most 5 s for a spool lock. The mirror held the
        # series and parity locks across the whole publication (reap, fetch,
        # push, confirmation and drain), 91 to 98 s on 2026-10-06, and
        # append-parity gave up on two runs' rows.
        self.append("run-parity")
        self.spool_parity()
        spools = (series.spool_dir(self.parent), series.parity_spool_dir(self.parent))
        original = mirror.publisher.publish
        entered: list[list[bool]] = []

        def free(spool: Path) -> bool:
            try:
                with series.series_spool_transition_lock(spool, exclusive=True, timeout_seconds=0):
                    return True
            except series.SeriesError:
                return False

        def probing_publish(*args, **kwargs):
            entered.append([free(spool) for spool in spools])
            return original(*args, **kwargs)

        with mock.patch.object(mirror.publisher, "publish", side_effect=probing_publish):
            report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        self.assertEqual(193, report["raw_publication"]["parity"]["published"])
        self.assertEqual([row("run-parity")], self.remote_rows())
        self.assertTrue(entered, "the publisher was not called")
        self.assertEqual([[True, True]] * len(entered), entered)

    def test_a_refused_parity_batch_never_holds_back_the_series_rows(self):
        parity = series.parity_ledger_module()
        events = parity.build_batch(
            self.parity_sources(), producer="validate", run_id=self.PARITY_RUN,
            hermit_sha=self.PARITY_TREE, emitted_at="2026-09-21T12:00:00Z",
            source_tree_dirty=False, host="fixture")
        events[1]["verdict"] = "matched"
        events[0]["verdicts"] = dict(events[0]["verdicts"], diverged=174, matched=1)
        batch = self.spool_parity(events)
        self.append("run-beside-refusal")
        report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("run-beside-refusal")], self.remote_rows())
        outcome = report["raw_publication"]["parity"]
        self.assertEqual((0, None, 1), (outcome["published"], outcome["commit"],
                                        outcome["pending_batches"]))
        refusal = (f"{batch.name}: event 2: parity ledger row {events[1]['cell']} "
                   f"(validate run {self.PARITY_RUN}): row verdict matched disagrees with "
                   "record verdict diverged")
        self.assertEqual([{"batch": batch.name, "error": refusal}], outcome["refused"])
        # The run's only batch cannot be read, so where it stands is unknown,
        # and the reason is named; it is never counted as published or empty.
        self.assertEqual({"producer": "validate", "run_id": self.PARITY_RUN,
                          "state": None, "records": None, "refusal": None}, outcome["run"])
        self.assertIn("cannot count parity: ParityLedgerError: " + refusal, outcome["error"])
        self.assertTrue(batch.exists(), "a refused parity batch stays spooled")
        self.assertIsNone(self.remote_file("parity/hermit/fixture/2026-09.jsonl"))
        self.run_registry().check_parity_publication(outcome)

    def test_a_refused_append_publishes_its_refusal_and_the_run_reads_refused(self):
        # https://github.com/rrnewton/hermit/issues/3301 finding [2]: a refused
        # append-parity is a durable record, never a run that reads absent.
        parity = series.parity_ledger_module()
        refusal = parity.build_refusal(
            producer="validate", run_id=self.PARITY_RUN, hermit_sha=self.PARITY_TREE,
            emitted_at="2026-09-21T12:00:00Z", source_tree_dirty=False, host="fixture",
            input_bytes=b"{}\n", input_lines=1,
            error="input row 1: parity ledger source fields are not exactly its schema's")
        spool = series.parity_spool_dir(self.parent)
        batch = spool / parity.append_refusal(spool, refusal)["batch"]
        self.append("run-beside-parity-refusal")
        report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        outcome = report["raw_publication"]["parity"]
        self.assertEqual((1, [batch.name], [], None), (outcome["published"], outcome["batches"],
                                                       outcome["refused"], outcome["error"]))
        refused_run = {"producer": "validate", "run_id": self.PARITY_RUN, "state": "refused",
                       "records": None, "refusal": refusal["error"]}
        self.assertEqual(refused_run, outcome["run"])
        self.assertEqual(parity.encode_batch([refusal]),
                         self.remote_file("parity-runs/hermit/fixture/2026-09.jsonl"))
        self.assertIsNone(self.remote_file("parity/hermit/fixture/2026-09.jsonl"))
        self.run_registry().check_parity_publication(outcome)
        # Once drained, the published refusal still names the run.
        again = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual(refused_run, again["raw_publication"]["parity"]["run"])
        # A marker for the run supersedes the refusal: a later attempt succeeded.
        self.spool_parity()
        self.append("run-parity-after-refusal")
        later = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual({"producer": "validate", "run_id": self.PARITY_RUN, "state": "published",
                          "records": 192, "refusal": None}, later["raw_publication"]["parity"]["run"])

    def append_parity_cli(self, data: bytes) -> tuple[int, str, str]:
        """`series.py append-parity` for the fixture run, with DATA on stdin."""
        stdin = io.TextIOWrapper(io.BytesIO(data), encoding="utf-8")
        with mock.patch.object(sys, "stdin", stdin), \
                contextlib.redirect_stdout(io.StringIO()) as out, \
                contextlib.redirect_stderr(io.StringIO()) as err:
            code = series.main([
                "append-parity", "--parent", str(self.parent), "--producer", "validate",
                "--run-id", self.PARITY_RUN, "--tree", self.PARITY_TREE,
                "--emitted-at", "2026-09-21T12:00:00Z", "--source-tree-dirty", "false"])
        return code, out.getvalue(), err.getvalue()

    def test_append_parity_finds_its_published_run_without_reading_the_whole_index(self):
        fixture = Path(__file__).resolve().parents[2] / "tests/fixtures/parity-run-1953"
        data = (fixture / "ledger-sources-typed.jsonl").read_bytes()
        self.assertEqual(0, self.append_parity_cli(data)[0])
        self.append("run-beside-parity")
        self.assertEqual("published", self.publish(publish_only=True)["state"])
        self.publish(publish_only=True)  # reaps the local published/ copy
        spool = series.parity_spool_dir(self.parent)
        self.assertEqual([], list(spool.glob("*.jsonl")) + list((spool / "published").glob("*.jsonl")))
        parity = series.parity_ledger_module()
        with mock.patch.object(parity, "published_parity_at_repo",
                               side_effect=AssertionError("read the whole parity index")):
            code, out, err = self.append_parity_cli(data)
            self.assertEqual(0, code, err)
            self.assertIn("already recorded", out + err)
            self.assertEqual([], list(spool.glob("*.jsonl")))
            # One row fewer under the same run is a changed record: refused.
            fewer = b"".join(data.splitlines(keepends=True)[:-1])
            code, out, err = self.append_parity_cli(fewer)
        self.assertEqual(1, code, out)
        self.assertIn("already recorded in the published ledger with different content", err)

    def defer_parity(self) -> Path:
        """Keep the run's append-parity call as `series.py append-parity` keeps one the spool did not take."""
        fixture = Path(__file__).resolve().parents[2] / "tests/fixtures/parity-run-1953"
        data = (fixture / "ledger-sources-typed.jsonl").read_bytes()
        return series.defer_parity_call(self.parent, {
            "schema": series.PARITY_DEFERRED_SCHEMA, "kind": "rows", "deferred_at": "2026-09-21T12:00:06Z",
            "reason": "timed out after 5.000s acquiring exclusive series spool lock",
            "producer": "validate", "run_id": self.PARITY_RUN, "tree": self.PARITY_TREE,
            "emitted_at": "2026-09-21T12:00:00Z", "source_tree_dirty": False, "host": "fixture",
            "input": data.decode(), "input_sha256": hashlib.sha256(data).hexdigest(),
            "input_bytes": len(data),
        })

    def test_a_deferred_parity_call_publishes_with_the_next_series_rows(self):
        # append-parity defers a call whose parity spool lock timed out; the
        # next publication replays it first, so the rows still arrive.
        kept = self.defer_parity()
        parity = series.parity_ledger_module()
        batch = parity.batch_name(parity.build_batch(
            self.parity_sources(), producer="validate", run_id=self.PARITY_RUN,
            hermit_sha=self.PARITY_TREE, emitted_at="2026-09-21T12:00:00Z",
            source_tree_dirty=False, host="fixture")[0])
        self.append("run-beside-deferred-parity")
        report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        self.assertEqual({"replayed": 1, "appended": 1, "already_recorded": 0, "refused": 0,
                          "batches": [batch], "kept": []}, report["raw_publication"]["parity_deferred"])
        outcome = report["raw_publication"]["parity"]
        self.assertEqual((193, [batch], [], None), (outcome["published"], outcome["batches"],
                                                    outcome["refused"], outcome["error"]))
        self.assertEqual({"producer": "validate", "run_id": self.PARITY_RUN, "state": "published",
                          "records": 192, "refusal": None}, outcome["run"])
        self.assertEqual([row("run-beside-deferred-parity")], self.remote_rows())
        self.assertEqual(192, len(self.remote_file("parity/hermit/fixture/2026-09.jsonl").splitlines()))
        self.assertFalse(kept.exists())
        self.assertEqual([], list(series.parity_deferred_dir(self.parent).iterdir()))
        self.run_registry().check_parity_publication(outcome)

    def test_a_parity_call_that_stays_deferred_reads_deferred_not_absent(self):
        parity = series.parity_ledger_module()
        directory = series.parity_deferred_dir(self.parent)
        directory.mkdir(parents=True)
        torn = directory / f"{parity.run_event_id('validate', self.PARITY_RUN)}.1-2-0badf00d.jsonl"
        torn.write_text('{"schema":"parity-deferred/v1"', encoding="utf-8")
        self.append("run-beside-kept-parity")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        reason = f"unreadable: {torn.name}: is not exactly one line"
        self.assertEqual({"replayed": 0, "appended": 0, "already_recorded": 0, "refused": 0,
                          "batches": [], "kept": [{"file": torn.name, "reason": reason}]},
                         report["raw_publication"]["parity_deferred"])
        self.assertIn(f"mirror: WARNING: the deferred parity call {torn.name} stays deferred: {reason}",
                      stderr.getvalue())
        outcome = report["raw_publication"]["parity"]
        self.assertEqual({"producer": "validate", "run_id": self.PARITY_RUN, "state": "deferred",
                          "records": None, "refusal": None}, outcome["run"])
        self.assertEqual([row("run-beside-kept-parity")], self.remote_rows())
        self.assertTrue(torn.exists(), "a deferred call that cannot be read is never deleted")
        self.run_registry().check_parity_publication(outcome)

    def test_another_runs_deferred_call_leaves_this_run_absent(self):
        parity = series.parity_ledger_module()
        directory = series.parity_deferred_dir(self.parent)
        directory.mkdir(parents=True)
        other = directory / f"{parity.run_event_id('validate', 'validate-another-run')}.1-2-0badf00d.jsonl"
        other.write_text('{"schema":"parity-deferred/v1"', encoding="utf-8")
        self.append("run-beside-another-runs-kept-parity")
        with contextlib.redirect_stderr(io.StringIO()):
            report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([other.name],
                         [item["file"] for item in report["raw_publication"]["parity_deferred"]["kept"]])
        outcome = report["raw_publication"]["parity"]
        self.assertEqual({"producer": "validate", "run_id": self.PARITY_RUN, "state": "absent",
                          "records": None, "refusal": None}, outcome["run"])
        self.assertEqual([row("run-beside-another-runs-kept-parity")], self.remote_rows())
        self.assertTrue(other.exists())
        self.run_registry().check_parity_publication(outcome)

    def test_a_publication_keeps_what_readmission_left_refused(self):
        # The series_quarantine health gate reads it (ledger_spool_backlog.py).
        self.append("run-beside-readmission")
        report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        readmission = report["raw_publication"]["readmission"]
        document = json.loads(series.readmission_summary_path(self.parent).read_text(encoding="utf-8"))
        self.assertEqual(
            (series.READMISSION_SUMMARY_SCHEMA, readmission["ledger_commit"], readmission["left_refused"]),
            (document["schema"], document["ledger_commit"], document["left_refused"]))
        self.assertRegex(document["ledger_commit"], r"\A[0-9a-f]{40}\Z")
        self.assertEqual([0, 0, 0, 0], [document["left_refused"][name]
                                        for name in series.READMISSION_INPUT_CLASSES])

    def test_a_readmission_summary_that_cannot_be_kept_is_a_warning(self):
        self.append("run-beside-unkept-summary")
        stderr = io.StringIO()
        with mock.patch.object(series, "write_readmission_summary",
                               side_effect=OSError(errno.ENOSPC, "No space left on device")) as kept, \
                contextlib.redirect_stderr(stderr):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("run-beside-unkept-summary")], self.remote_rows())
        readmission = report["raw_publication"]["readmission"]
        self.assertEqual([mock.call(self.parent, readmission)], kept.call_args_list)
        self.assertIn("mirror: WARNING: keeping the readmission summary failed: "
                      "OSError: [Errno 28] No space left on device", stderr.getvalue())
        self.assertFalse(series.readmission_summary_path(self.parent).exists())

    def test_a_failed_readmission_leaves_the_last_summary_to_age(self):
        path = series.write_readmission_summary(self.parent, {
            "ledger_commit": "a" * 40,
            "left_refused": series._left_refused_summary({
                name: {"projection: fixture refusal": 3} if name == "missing_with_evidence" else {}
                for name in series.READMISSION_INPUT_CLASSES}),
        })
        last = path.read_bytes()
        self.append("run-beside-failed-readmission")
        stderr = io.StringIO()
        with mock.patch.object(series, "readmit_quarantined",
                               side_effect=series.SeriesError("fixture: ledger unreadable")), \
                contextlib.redirect_stderr(stderr):
            report = self.publish(publish_only=True)
        self.assertEqual("published", report["state"], report)
        self.assertEqual([row("run-beside-failed-readmission")], self.remote_rows())
        self.assertEqual({"error": "SeriesError: fixture: ledger unreadable"},
                         report["raw_publication"]["readmission"])
        self.assertIn("mirror: WARNING: replaying refused series inputs failed: "
                      "SeriesError: fixture: ledger unreadable", stderr.getvalue())
        self.assertEqual(last, path.read_bytes())

    def test_scheduled_backlog_publishes_a_deferred_parity_call_alone(self):
        # With nothing else spooled, the deferred call is still parity backlog:
        # the sweep runs the parity store, whose mirror replays and publishes it.
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )
        native_run = subprocess.run

        def sweep_child(command, **kwargs):
            if str(MODULE) in command:
                command = bounded_fixture_command(
                    [sys.executable, "-B", "-c", bootstrap, *command[1:]])
            return native_run(command, **kwargs)

        kept = self.defer_parity()
        self.assertEqual({"parity": series.parity_spool_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))
        with mock.patch.object(publish_backlog, "STATE_ROOT", self.parent), \
             mock.patch.object(publish_backlog.subprocess, "run", side_effect=sweep_child):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                dry = publish_backlog.main(["--dry-run", "--net-wrapper", "/usr/bin/env"])
            self.assertEqual(0, dry)
            self.assertEqual({str(self.parent): {"parity": 1}},
                             json.loads(output.getvalue())["would_publish"])
            self.assertTrue(kept.exists(), "a dry run replays nothing")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                passed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
        delivered = json.loads(output.getvalue())
        self.assertEqual(0, passed, delivered)
        [store] = delivered["reports"]
        self.assertEqual(("parity", 0, 193), (store["store"], store["rc"], store["published"]))
        self.assertEqual(1, store["raw_publication"]["parity_deferred"]["appended"])
        self.assertEqual(192, len(self.remote_file("parity/hermit/fixture/2026-09.jsonl").splitlines()))
        self.assertFalse(kept.exists())
        self.assertEqual({}, publish_backlog.pending_stores(self.parent))

    def test_scheduled_backlog_fails_the_parity_store_while_a_call_stays_kept(self):
        # A deferred call the replay cannot take leaves the parity spool empty
        # and the mirror exit 0. The sweep still fails the parity store, and the
        # call stays for the next publication.
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series;"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )
        native_run = subprocess.run

        def sweep_child(command, **kwargs):
            if str(MODULE) in command:
                command = bounded_fixture_command(
                    [sys.executable, "-B", "-c", bootstrap, *command[1:]])
            return native_run(command, **kwargs)

        parity = series.parity_ledger_module()
        directory = series.parity_deferred_dir(self.parent)
        directory.mkdir(parents=True)
        torn = directory / f"{parity.run_event_id('validate', self.PARITY_RUN)}.1-2-0badf00d.jsonl"
        torn.write_text('{"schema":"parity-deferred/v1"', encoding="utf-8")
        self.assertEqual({"parity": series.parity_spool_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))
        with mock.patch.object(publish_backlog, "STATE_ROOT", self.parent), \
             mock.patch.object(publish_backlog.subprocess, "run", side_effect=sweep_child):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                failed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
        delivered = json.loads(output.getvalue())
        self.assertEqual(1, failed, delivered)
        self.assertEqual((1, 1, 0), (delivered["failed_stores"], delivered["failed_parents"],
                                     delivered["published"]))
        [store] = delivered["reports"]
        reason = f"unreadable: {torn.name}: is not exactly one line"
        self.assertEqual(("parity", 1, 0), (store["store"], store["rc"], store.get("published")), store)
        self.assertEqual(f"1 kept parity call(s) not replayed: {torn.name}: {reason}", store["error"])
        self.assertEqual([{"file": torn.name, "reason": reason}],
                         store["raw_publication"]["parity_deferred"]["kept"])
        self.assertTrue(torn.exists(), "a deferred call that cannot be read is never deleted")
        self.assertEqual({"parity": series.parity_spool_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))

    def test_scheduled_backlog_fails_the_parity_store_when_the_series_fail_beside_a_kept_call(self):
        # The series publication fails (a series spool lock timeout raises the
        # same PublishRefused), so the mirror exits 1, publishes the parity
        # batch alone, and prints its failure report. The parity store is
        # judged by that report's sub-reports; it must carry the replay, which
        # kept another run's call. Without it the store read as published and
        # the sweep passed while the call was still off the ledger.
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/ledger"))
        import publish_backlog

        bootstrap = (
            "import runpy,sys,subprocess;"
            f"sys.path[:0]=[{str(MODULE.parent)!r},{str(mirror.TOOL_ROOT / 'ci-hub/ledger')!r}];"
            "import series,publisher;"
            "series._run_shared_checker=lambda *a: subprocess.CompletedProcess(a,0,'','');"
            "publisher._series_store=lambda:series\n"
            "def refuse_series(*args, publish=publisher.publish, **kwargs):\n"
            "    if kwargs.get('store') == 'series':\n"
            "        raise publisher.PublishRefused('deliberate series refusal; spool retained')\n"
            "    return publish(*args, **kwargs)\n"
            "publisher.publish=refuse_series\n"
            "sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"
        )
        native_run = subprocess.run

        def sweep_child(command, **kwargs):
            if str(MODULE) in command:
                command = bounded_fixture_command(
                    [sys.executable, "-B", "-c", bootstrap, *command[1:]])
            return native_run(command, **kwargs)

        parity = series.parity_ledger_module()
        batch = self.spool_parity()
        directory = series.parity_deferred_dir(self.parent)
        directory.mkdir(parents=True)
        torn = directory / f"{parity.run_event_id('validate', 'validate-another-run')}.1-2-0badf00d.jsonl"
        torn.write_text('{"schema":"parity-deferred/v1"', encoding="utf-8")
        self.assertEqual({"parity": series.parity_spool_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))
        with mock.patch.object(publish_backlog, "STATE_ROOT", self.parent), \
             mock.patch.object(publish_backlog.subprocess, "run", side_effect=sweep_child):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                failed = publish_backlog.main(["--net-wrapper", "/usr/bin/env", "--timeout-seconds", "30"])
        delivered = json.loads(output.getvalue())
        self.assertEqual(1, failed, delivered)
        self.assertEqual((1, 1, 193), (delivered["failed_stores"], delivered["failed_parents"],
                                       delivered["published"]))
        [store] = delivered["reports"]
        reason = f"unreadable: {torn.name}: is not exactly one line"
        self.assertEqual(("parity", 1, 193, "failed"),
                         (store["store"], store["rc"], store["published"], store["state"]), store)
        self.assertEqual(f"1 kept parity call(s) not replayed: {torn.name}: {reason}", store["error"])
        self.assertEqual([{"file": torn.name, "reason": reason}],
                         store["raw_publication"]["parity_deferred"]["kept"])
        alone = store["raw_publication"]["parity"]
        self.assertEqual((193, False, [batch.name], None),
                         (alone["published"], alone["joint"], alone["batches"], alone["error"]))
        self.assertEqual("parity: append 193 event(s)", git(self.remote, "log", "-1", "--format=%s", "main"))
        self.assertEqual([], self.remote_rows())
        self.assertFalse(batch.exists())
        self.assertTrue(torn.exists(), "a deferred call that cannot be read is never deleted")
        self.assertEqual({"parity": series.parity_spool_dir(self.parent)},
                         publish_backlog.pending_stores(self.parent))

    def test_a_successful_publication_records_a_typed_parity_outcome_or_names_the_misfit(self):
        # https://github.com/rrnewton/hermit/issues/3301 finding [7]: the
        # success path checks raw_publication.parity as the failed path does.
        self.append("run-parity-success")
        self.spool_parity()
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertEqual("published", report["scorecard"]["state"], report)
        parity = report["raw_publication"]["parity"]
        self.assertEqual((193, True), (parity["published"], parity["joint"]))
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        import start_unit
        run_registry = self.run_registry()
        no_gh = self.root / "no-gh"
        no_gh.write_text("#!/bin/sh\necho 'GitHub must not be called' >&2\nexit 97\n")
        no_gh.chmod(0o755)
        for index, (name, carried) in enumerate([("fits", parity),
                                                 ("unfit parity", {**parity, "joint": "no"})]):
            with self.subTest(name):
                printed = {**report, "raw_publication": {**report["raw_publication"], "parity": carried}}

                def succeeded_mirror(command, **kwargs):
                    return subprocess.CompletedProcess(
                        command, 0, json.dumps(printed, sort_keys=True) + "\n", "")

                record_path = self.parent / f"parity-success-{index}.json"
                record = {
                    "schema_version": run_registry.SCHEMA_VERSION,
                    "producer": "normal-completion-publication-fixture",
                    "repo": "rrnewton/hermit", "checkout": str(self.source),
                    "target": self.target, "canonical_verdict": "VALIDATED",
                    "unit": self.PARITY_RUN + ".service", "wrapper_exit_code": 0,
                    "e2e_result_root": str(self.root / "results"),
                    "github_executable": str(no_gh),
                }
                run_registry.write_record(record_path, record)
                stderr = io.StringIO()
                with mock.patch.dict(os.environ), contextlib.redirect_stderr(stderr):
                    os.environ.pop("FLEET_REAL_GH", None)
                    result = start_unit.finalize_ledger_mirror(
                        self.parent, mirror.TOOL_ROOT, record_path, record=record,
                        run=succeeded_mirror)
                self.assertEqual(result, run_registry.read_record(record_path))
                mirrored = result["ledger_mirror"]
                self.assertEqual("published", mirrored["state"], mirrored)
                # What was written is what the record's own schema accepts.
                run_registry._ledger_mirror(mirrored)
                others = {k: v for k, v in report["raw_publication"].items() if k != "parity"}
                if name == "fits":
                    self.assertEqual({**others, "parity": parity}, mirrored["raw_publication"])
                    self.assertNotIn("PARITY OUTCOME NOT RECORDED", stderr.getvalue())
                else:
                    reason = ("validation-run-handle-ledger_mirror.raw_publication.parity.joint: "
                              "must be a boolean")
                    self.assertEqual({**others, "parity_not_recorded": reason},
                                     mirrored["raw_publication"])
                    self.assertIn(f"validate-run: PARITY OUTCOME NOT RECORDED: {reason}",
                                  stderr.getvalue())

    def test_a_scorecard_without_parity_json_is_tolerated_and_one_with_it_is_bound(self):
        self.append("run-no-parity-json")
        report = self.publish()
        self.assertEqual("published", report["state"], report)
        self.assertEqual({"scorecard/SCORECARD.md", "scorecard/cells.json"},
                         set(report["scorecard"]["files"]))
        self.assertIsNone(self.remote_file("scorecard/parity.json"))
        # A hermit new enough to summarize the parity ledger writes parity.json.
        tool = self.tool / "ci/compat-envelope/scorecard.rs"
        tool.write_text(tool.read_text() + "(p/'parity.json').write_text('{\"schema\":\"parity-summary/v1\"}\\n')\n")
        git(self.tool, "commit", "-qam", "scorecard tool that writes parity.json")
        self.append("run-parity-json")
        bound = self.publish()
        self.assertEqual("published", bound["state"], bound)
        files = bound["scorecard"]["files"]
        self.assertEqual({"scorecard/SCORECARD.md", "scorecard/cells.json", "scorecard/parity.json"},
                         set(files))
        text = '{"schema":"parity-summary/v1"}\n'
        self.assertEqual(text, self.remote_file("scorecard/parity.json"))
        self.assertEqual(hashlib.sha256(text.encode()).hexdigest(), files["scorecard/parity.json"])
        self.assertEqual(files["scorecard/parity.json"],
                         self.remote_latest()["scorecard"]["scorecard/parity.json"])
        run_registry = self.run_registry()
        run_registry._scorecard_publication_files(files)
        # Only parity.json is optional; any other extra path is still refused,
        # and a portable proof never binds parity.json.
        with self.assertRaisesRegex(RuntimeError, "contains an unsupported publication path"):
            run_registry._scorecard_publication_files({**files, "scorecard/other.json": "0" * 64})
        with self.assertRaisesRegex(RuntimeError, "contains an unsupported publication path"):
            run_registry._scorecard_publication_files(
                {"scorecard/parity.json": files["scorecard/parity.json"]},
                generated=frozenset(), optional=frozenset())

    def test_a_failed_series_push_publishes_parity_alone_and_the_record_keeps_it(self):
        hook = self.remote / "hooks/pre-receive"
        hook.write_text(
            "#!/bin/sh\nwhile read old new ref; do\n"
            " if git diff --name-only \"$old\" \"$new\" | grep -q '^series/'; then\n"
            "  echo deliberate-series-refusal >&2; exit 1\n fi\ndone\nexit 0\n"
        )
        hook.chmod(0o755)
        series_batch = self.append("run-series-refused")
        batch = self.spool_parity()
        with self.assertRaises(mirror.SeriesPublicationFailed) as caught:
            self.publish(publish_only=True, attempts=1, finalized_run_id=self.PARITY_RUN)
        parity = caught.exception.parity
        head = git(self.remote, "rev-parse", "main")
        self.assertEqual((193, False, head, [batch.name], None),
                         (parity["published"], parity["joint"], parity["commit"],
                          parity["batches"], parity["error"]))
        self.assertEqual("published", parity["run"]["state"])
        self.assertEqual("parity: append 193 event(s)", git(self.remote, "log", "-1", "--format=%s", "main"))
        self.assertEqual([], self.remote_rows())
        self.assertTrue(series_batch.exists() and not batch.exists())

        failure = mirror.failure_report(self.parent, caught.exception,
                                        run_id=self.PARITY_RUN, count=True)
        self.assertEqual(parity, failure["raw_publication"]["parity"])
        # The replay ran before the rows failed, and the failure carries it.
        replay = {"replayed": 0, "appended": 0, "already_recorded": 0, "refused": 0,
                  "batches": [], "kept": []}
        self.assertEqual(replay, caught.exception.parity_deferred)
        self.assertEqual(replay, failure["raw_publication"]["parity_deferred"])
        sys.path.insert(0, str(mirror.TOOL_ROOT / "ci-hub/validate"))
        import start_unit
        run_registry = self.run_registry()
        no_gh = self.root / "no-gh"
        no_gh.write_text("#!/bin/sh\necho 'GitHub must not be called' >&2\nexit 97\n")
        no_gh.chmod(0o755)
        quarantine = failure["raw_publication"]["quarantine"]
        unfit_parity = ("parity outcome not recorded: "
                        "validation-run-handle-ledger_mirror.raw_publication.parity.joint")
        unfit_replay = ("parity replay outcome not recorded: "
                        "validation-run-handle-ledger_mirror.raw_publication.parity_deferred.replayed")
        # (printed failure report, what the record keeps, what its detail names)
        cases = [
            ("fits", failure["raw_publication"],
             {"quarantine": quarantine, "parity": parity, "parity_deferred": replay}, None),
            # An unfit outcome is dropped and named; the outcomes beside it are kept.
            ("unfit parity", {**failure["raw_publication"], "parity": {**parity, "joint": "no"}},
             {"quarantine": quarantine, "parity_deferred": replay}, unfit_parity),
            ("unfit replay", {**failure["raw_publication"], "parity_deferred": {**replay, "replayed": 1}},
             {"quarantine": quarantine, "parity": parity}, unfit_replay),
            ("a mirror without the replay", {"quarantine": quarantine, "parity": parity},
             {"quarantine": quarantine, "parity": parity}, None),
        ]
        for index, (name, raw_publication, recorded, named) in enumerate(cases):
            with self.subTest(name):
                printed = {**failure, "raw_publication": raw_publication}

                def failed_mirror(command, **kwargs):
                    return subprocess.CompletedProcess(
                        command, 1, json.dumps(printed, sort_keys=True) + "\n",
                        "ledger publication: FAILED: deliberate-series-refusal\n")

                record_path = self.parent / f"parity-failure-{index}.json"
                record = {
                    "schema_version": run_registry.SCHEMA_VERSION,
                    "producer": "normal-completion-publication-fixture",
                    "repo": "rrnewton/hermit", "checkout": str(self.source),
                    "target": self.target, "canonical_verdict": "VALIDATED",
                    "unit": self.PARITY_RUN + ".service", "wrapper_exit_code": 0,
                    "e2e_result_root": str(self.root / "results"),
                    "github_executable": str(no_gh),
                }
                run_registry.write_record(record_path, record)
                with mock.patch.dict(os.environ):
                    os.environ.pop("FLEET_REAL_GH", None)
                    result = start_unit.finalize_ledger_mirror(
                        self.parent, mirror.TOOL_ROOT, record_path, record=record,
                        run=failed_mirror)
                self.assertEqual(result, run_registry.read_record(record_path))
                mirrored = result["ledger_mirror"]
                self.assertEqual("failed", mirrored["state"], mirrored)
                run_registry._ledger_mirror(mirrored)
                self.assertEqual(recorded, mirrored["raw_publication"])
                if named is None:
                    self.assertNotIn("outcome not recorded", mirrored["detail"])
                else:
                    self.assertIn(named, mirrored["detail"])
                    self.assertEqual(1, mirrored["detail"].count("outcome not recorded"), mirrored["detail"])

    def test_recovery_skips_an_unreadable_parity_batch_the_unpublished_commit_does_not_need(self):
        # A crash between commit and push left the writer on a parity commit
        # whose events are all in a readable spooled batch. An unrelated
        # spooled batch that cannot be read no longer blocks its recovery: the
        # publisher names that batch as refused and it stays spooled.
        parity = series.parity_ledger_module()
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        batch = self.spool_parity()
        marker, rows = parity.read_batch(batch)
        crashed = self.commit_parity(writer, [marker, *rows])
        torn = self.spool_torn_refusal("validate-unrelated-run")
        with self.assertRaises(parity.ParityLedgerError) as unreadable:
            parity.read_batch(torn)
        self.assertEqual(f"{torn.name}: final line has no newline terminator", str(unreadable.exception))
        self.append("run-beside-unreadable-parity")
        report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        recovered = report["raw_publication"]["recovered_ref"]
        self.assertEqual(f"refs/heads/recovery/cell-series-{crashed}", recovered)
        self.assertEqual(crashed, git(writer, "rev-parse", recovered))
        head = git(self.remote, "rev-parse", "main")
        self.assertEqual({
            "schema": "parity-publication/v1", "published": 193, "already_published": 0,
            "commit": head, "joint": True, "batches": [batch.name],
            "refused": [{"batch": torn.name, "error": str(unreadable.exception)}],
            "error": None, "pending_batches": 1, "historic_invalid": 0,
            "run": {"producer": "validate", "run_id": self.PARITY_RUN,
                    "state": "published", "records": 192, "refusal": None},
        }, report["raw_publication"]["parity"])
        self.assertEqual([row("run-beside-unreadable-parity")], self.remote_rows())
        self.assertEqual(192, len(self.remote_file("parity/hermit/fixture/2026-09.jsonl").splitlines()))
        self.assertTrue(torn.exists() and not batch.exists(), "the unreadable batch stays spooled")

    def test_recovery_refuses_a_parity_event_whose_only_spooled_copy_is_unreadable(self):
        # Skipping an unreadable batch still fails closed. The unpublished
        # commit adds events whose only spooled copy is a batch torn before
        # its final newline, so they are not pending and the commit is kept.
        parity = series.parity_ledger_module()
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        batch = self.spool_parity()
        marker, rows = parity.read_batch(batch)
        crashed = self.commit_parity(writer, [marker, *rows])
        batch.write_text(parity.encode_batch([marker, *rows])[:-1], encoding="utf-8")
        with self.assertRaises(parity.ParityLedgerError) as unreadable:
            parity.read_batch(batch)
        self.assertEqual(f"{batch.name}: final line has no newline terminator", str(unreadable.exception))
        series_batch = self.append("run-beside-torn-parity")
        with self.assertRaises(mirror.MirrorRefused) as caught:
            self.publish(publish_only=True)
        self.assertEqual(
            f"unpublished ledger commit {crashed} is not an unchanged spooled parity append; retained",
            str(caught.exception))
        self.assertEqual(crashed, git(writer, "rev-parse", "HEAD"))
        self.assertEqual([], self.remote_rows())
        self.assertIsNone(self.remote_file("parity/hermit/fixture/2026-09.jsonl"))
        self.assertTrue(batch.exists() and series_batch.exists())

    def test_recovery_reads_only_the_parity_shards_the_commit_changed(self):
        # Reading the whole published parity ledger at both commits held two
        # complete namespaces: 28.3 GB in one publication on 2026-10-11.
        parity = series.parity_ledger_module()
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        batch = self.spool_parity()
        marker, rows = parity.read_batch(batch)
        crashed = self.commit_parity(writer, [marker, *rows])
        changed = git(writer, "diff", "--name-only", "refs/remotes/origin/main", crashed).splitlines()
        real = parity.published_parity_shards
        calls = []

        def published_parity_shards(repo, ref, shards):
            calls.append((ref, sorted(shards)))
            return real(repo, ref, shards)

        self.append("run-beside-recovered-parity")
        with mock.patch.object(parity, "published_parity_shards", side_effect=published_parity_shards):
            report = self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)
        self.assertEqual("published", report["state"], report)
        self.assertEqual(f"refs/heads/recovery/cell-series-{crashed}",
                         report["raw_publication"]["recovered_ref"])
        self.assertEqual(2, len(calls), calls)
        self.assertEqual([crashed], [ref for ref, _ in calls][1:])
        for _ref, shards in calls:
            self.assertEqual(sorted(path for path in changed if parity.is_shard_path(path)), shards)

    def test_recovery_counts_a_published_event_copied_into_another_part_as_added(self):
        # The one case the shard-limited check judges differently: an event
        # already published in a shard the commit did not change, copied into
        # one it did. It is not pending in the spool, so the commit is kept
        # (the whole-ledger check saw it as already published and accepted it).
        parity = series.parity_ledger_module()
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        self.spool_parity()
        self.append("run-publishing-parity")
        self.assertEqual("published", self.publish(publish_only=True, finalized_run_id=self.PARITY_RUN)["state"])
        published = parity.published_parity_at_repo(writer, "refs/remotes/origin/main")
        event = next(event for event in published.events.values()
                     if event.get("event_type") == "parity.record")
        whole = parity.event_shard(event)
        copy = whole[: -len(".jsonl")] + ".part-0001.jsonl"
        self.assertTrue(parity.is_shard_path(copy), copy)
        (writer / copy).write_text(parity.canonical_line(event) + "\n", encoding="utf-8")
        git(writer, "add", "--", copy)
        git(writer, "commit", "-qm", "parity: append 1 event(s)")
        crashed = git(writer, "rev-parse", "HEAD")
        series_batch = self.append("run-beside-copied-parity")
        with self.assertRaises(mirror.MirrorRefused) as caught:
            self.publish(publish_only=True)
        self.assertEqual(
            f"unpublished ledger commit {crashed} is not an unchanged spooled parity append; retained",
            str(caught.exception))
        self.assertEqual(crashed, git(writer, "rev-parse", "HEAD"))
        self.assertTrue(series_batch.exists())

    def test_recovery_refuses_when_the_parity_state_of_either_commit_cannot_be_read(self):
        # An error reading the parity state at the published base or at the
        # unpublished commit is still a refusal; only spooled batches are
        # skipped when they cannot be read.
        parity = series.parity_ledger_module()
        self.publish(publish_only=True)
        writer = self.parent / mirror.WRITER_RELATIVE
        batch = self.spool_parity()
        marker, rows = parity.read_batch(batch)
        crashed = self.commit_parity(writer, [marker, *rows])
        base = git(writer, "rev-parse", "refs/remotes/origin/main")
        series_batch = self.append("run-beside-unreadable-parity-state")
        real = parity.published_parity_shards
        for name, unreadable in (("published base", base), ("unpublished commit", crashed)):
            with self.subTest(name):
                error = f"cannot read {unreadable}:parity/hermit/fixture/2026-09.jsonl: fixture: object missing"

                def published_parity_shards(repo, ref, shards):
                    if ref == unreadable:
                        raise parity.ParityLedgerError(error)
                    return real(repo, ref, shards)

                with mock.patch.object(parity, "published_parity_shards",
                                       side_effect=published_parity_shards), \
                        self.assertRaises(mirror.MirrorRefused) as caught:
                    self.publish(publish_only=True)
                self.assertEqual(f"cannot check unpublished parity at {crashed}: {error}",
                                 str(caught.exception))
                self.assertEqual(crashed, git(writer, "rev-parse", "HEAD"))
                self.assertEqual([], self.remote_rows())
                self.assertIsNone(self.remote_file("parity/hermit/fixture/2026-09.jsonl"))
                self.assertTrue(batch.exists() and series_batch.exists())

    # --- A retry of a run whose scorecard already published ---------------
    # One ops tick runs the publisher up to three times for one run (in the
    # validation, from the unit's stop-post, from the launcher). A retry that
    # finds this run's complete scorecard transaction on main reuses it; one
    # that cannot prove it renders exactly as before.

    HISTORY_EDIT = "history-edit.json"

    def use_history_renderer(self) -> Path:
        """Install a renderer that writes the history scorecard.rs writes, from its real inputs.

        project-and-observe-results records the snapshot's source, commit,
        tree and row count, the refresh time, and a comparison binding of the
        finalized run at the measured commit and rows; this renderer writes
        those from the arguments it is given. A JSON edit file at the returned
        path forges single fields ("set") or the whole document ("raw"). It
        also prints, in scorecard.rs's formats, the first three snapshot rows
        as skipped, for two reasons, and one skipped cell.
        """
        edit = self.root / self.HISTORY_EDIT
        self.set_tool(
            "import json\n"
            "args=sys.argv[1:]\n"
            "assert args[0]=='project-and-observe-results', args\n"
            "a=dict(zip(args[1::2],args[2::2]))\n"
            "snap=json.loads(pathlib.Path(a['--snapshot']).read_text())\n"
            "run=json.loads(pathlib.Path(a['--finalized-row']).read_text())\n"
            "s=snap['source']\n"
            "binding={'run_id':run['run_id'],'hermit_sha':a['--expected-head'],'kind':'current',"
            "'snapshot':{'repository':s['repository'],'source':s['path'],'commit':s['commit'],"
            "'tree':s['tree'],'rows_sha256':snap['rows_sha256']}}\n"
            "history={'schema':8,'projection':{'source':s['path'],'source_repository':s['repository'],"
            "'source_commit':s['commit'],'source_tree':s['tree'],'refreshed_at':a['--refreshed-at'],"
            "'rows_read':snap['rows_read'],'pre_series_corpus':False,'comparison_attempt_bindings_v1':"
            "{'schema':1,'authority':'fixture','bindings':[binding]}},'cells':[]}\n"
            f"edit=pathlib.Path({str(edit)!r})\n"
            "spec=json.loads(edit.read_text()) if edit.exists() else {}\n"
            "for path,value in spec.get('set',[]):\n"
            " holder=history\n"
            " for key in path[:-1]: holder=holder[key]\n"
            " holder[path[-1]]=value\n"
            "(p/'cells.json').write_text(spec.get('raw',json.dumps(history,indent=1)+'\\n'))\n"
            "(p/'SCORECARD.md').write_text('rendered from '+s['commit']+'\\n')\n"
            "for i,(r,reason) in enumerate(zip(snap['rows'],['reason one','reason two','reason one'])):\n"
            " print(f\"  skipped {a['--snapshot']}:rows[{i}]:{r['event_id']}: {r['series']['cell']} from"
            " {r['producer']} run {r['run_id']} repetition {r['series']['run_index']} @"
            " {r['series']['tree']}: {reason}\")\n"
            "print('  skipped c-programs/x/verify@ptrace: outcome \"errored\" produced no comparison')\n")
        return edit

    def schema10_doubles(self, encoded, selection):
        actual_publish = mirror.publisher.publish

        def publish(*args, **kwargs):
            if kwargs.get("store") == "ledger":
                return {"published": 1}
            return actual_publish(*args, **kwargs)

        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(mirror, "finalized_producer_row", return_value=encoded))
        stack.enter_context(mock.patch.object(mirror, "_parent_tool_commit", return_value="8" * 40))
        stack.enter_context(mock.patch.object(mirror, "_select_published_terminal",
                                              return_value=(mock.MagicMock(), selection)))
        stack.enter_context(mock.patch.object(mirror.publisher, "publish", side_effect=publish))
        return stack

    def publish_schema10(self, encoded, selection, **patches):
        stderr = io.StringIO()
        with self.schema10_doubles(encoded, selection), contextlib.ExitStack() as extra, \
                contextlib.redirect_stderr(stderr):
            for name, value in patches.items():
                extra.enter_context(mock.patch.object(mirror, name, value))
            report = self.publish(results=self.root / "results", finalized_run_id=selection["row"]["run_id"])
        return report, stderr.getvalue()

    def append_naming(self, identifier: str, run_id: str) -> Path:
        value = row(identifier)
        value["run_id"] = run_id
        path = series.spool_dir(self.parent) / f"{identifier}.jsonl"
        series.append_cell_rows(self.parent, path, [value])
        return path

    def captured_check(self, publish):
        """Run PUBLISH with the retry check captured; return its arguments and outcome."""
        seen = {}
        real = mirror.published_run_scorecard

        def capture(writer, snapshot, **kwargs):
            # Later adversarial checks outlive the publication's private
            # input scope. Hold their own exact-commit authority until this
            # test's cleanup, while the real call uses its original owner.
            held = mirror._published_snapshot(writer, snapshot["source"]["commit"])
            self.addCleanup(mirror._close_published_snapshot, held)
            self.assertEqual(snapshot["source"], held["source"])
            self.assertEqual(snapshot["rows_sha256"], held["rows_sha256"])
            seen.update(writer=writer, snapshot=held, **kwargs)
            return real(writer, snapshot, **kwargs)

        report, stderr = publish(capture)
        return seen, report, stderr

    def receipt_path(self, run_id: str) -> Path:
        return mirror._receipt_path(self.parent, run_id)

    def test_retry_of_a_published_run_reuses_its_scorecard_transaction(self):
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        run_id = selection["row"]["run_id"]
        for identifier in ("portable-cell", "skipped-cell-1", "skipped-cell-2"):
            self.append(identifier)
        first, stderr = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["state"], first)
        self.assertEqual("published", first["scorecard"]["state"], first)
        transaction = first["commit"]
        self.assertEqual(transaction, git(self.remote, "rev-parse", "main"))
        self.assertEqual(first["series_commit"], git(self.remote, "rev-parse", transaction + "^"))
        # The render's completion is recorded on this host, beside the writer.
        receipt = self.receipt_path(run_id)
        self.assertEqual(self.parent / mirror.RECEIPTS_RELATIVE, receipt.parent)
        self.assertFalse(receipt.is_relative_to(self.parent / mirror.WRITER_RELATIVE))
        self.assertEqual({
            "schema": mirror.RECEIPT_SCHEMA, "run_id": run_id, "hermit_commit": self.target,
            "tool_hermit_commit": git(self.tool, "rev-parse", "HEAD"), "transaction_commit": transaction,
            "series_commit": first["series_commit"],
            "latest_sha256": hashlib.sha256(subprocess.check_output(
                ["git", "-C", str(self.remote), "show", f"{transaction}:latest.json"])).hexdigest(),
            "finalized_row_sha256": hashlib.sha256(encoded).hexdigest(),
            "files": first["scorecard"]["files"],
        }, json.loads(receipt.read_text()))
        self.assertIn(f"scorecard completion receipt {receipt} names transaction {transaction}", stderr)
        # The render's skipped snapshot rows arrive as one summary with exact
        # counts; the skipped cell is relayed as it was printed.
        self.assertRegex(stderr, r"  skipped 3 row\(s\) of snapshot \S+snapshot\.json \(sha256 [0-9a-f]{64}, "
                                 rf"ledger commit {first['series_commit']}\), reproducible with scorecard tool "
                                 rf"{git(self.tool, 'rev-parse', 'HEAD')} on measured source {self.target}, "
                                 r"for 2 reason\(s\)")
        self.assertIn("    2 row(s): reason one\n      first: skipped ", stderr)
        self.assertIn("    1 row(s): reason two\n      first: skipped ", stderr)
        self.assertNotIn(":rows[2]:", stderr)
        self.assertIn('  skipped c-programs/x/verify@ptrace: outcome "errored" produced no comparison\n', stderr)
        render = mock.Mock(side_effect=AssertionError("the published scorecard was rendered again"))

        again, stderr = self.publish_schema10(encoded, selection, build_scorecard=render)
        render.assert_not_called()
        self.assertEqual("unchanged", again["state"], again)
        self.assertEqual(transaction, git(self.remote, "rev-parse", "main"))
        self.assertEqual({"state": "published", "files": first["scorecard"]["files"],
                          "hermit_commit": self.target,
                          "tool_hermit_commit": git(self.tool, "rev-parse", "HEAD"),
                          "transaction_commit": transaction}, again["scorecard"])
        self.assertNotIn("portable_proof", again)
        self.assertIn(f"SCORECARD ALREADY PUBLISHED by run {selection['row']['run_id']}: "
                      f"published at {transaction}", stderr)

        # Rows from another run publish and still reuse the transaction: the
        # retry's cost follows what this run changed, not the whole ledger.
        self.append("another-runs-cell")
        later, stderr = self.publish_schema10(encoded, selection, build_scorecard=render)
        render.assert_not_called()
        self.assertEqual("published", later["state"], later)
        self.assertEqual(transaction, later["scorecard"]["transaction_commit"])
        self.assertEqual(later["commit"], git(self.remote, "rev-parse", "main"))
        self.assertNotEqual(transaction, later["commit"])
        self.assertEqual(["portable-cell", "skipped-cell-1", "skipped-cell-2", "another-runs-cell"],
                         [r["event_id"] for r in self.remote_rows()])
        self.assertIn("SCORECARD ALREADY PUBLISHED", stderr)

    def test_a_scorecard_transaction_rebuilt_after_a_lost_push_is_reused_by_a_retry(self):
        # The transaction is committed again on another host's appended rows
        # (_push_rebuilt_over_appends); its latest.json and the receipt still
        # name the rows it was rendered from.
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        run_id = selection["row"]["run_id"]
        self.append("portable-cell")
        with self._another_host_pushes_first(self._appending("elsewhere")) as racers:
            first, stderr = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["state"], first)
        self.assertEqual("published", first["scorecard"]["state"], first)
        transaction, series_commit = first["commit"], first["series_commit"]
        self.assertEqual(transaction, git(self.remote, "rev-parse", "main"))
        self.assertEqual([transaction, racers[0]],
                         git(self.remote, "rev-list", "--parents", "-n", "1", transaction).split())
        self.assertEqual(series_commit, git(self.remote, "rev-parse", racers[0] + "^"))
        latest = json.loads(git(self.remote, "show", f"{transaction}:latest.json"))
        self.assertEqual((series_commit, series_commit), (latest["source_commit"], latest["series_commit"]))
        receipt = json.loads(self.receipt_path(run_id).read_text())
        self.assertEqual((transaction, series_commit), (receipt["transaction_commit"], receipt["series_commit"]))
        self.assertIn(f"rebuilt as {transaction} on {racers[0]}, which main reached from {series_commit} "
                      "by appends only", stderr)
        self.assertIn(f"scorecard completion receipt {self.receipt_path(run_id)} names transaction "
                      f"{transaction}", stderr)
        self.assertEqual(["portable-cell", "elsewhere"], [r["event_id"] for r in self.remote_rows()])
        render = mock.Mock(side_effect=AssertionError("the published scorecard was rendered again"))

        seen, again, stderr = self.captured_check(
            lambda capture: self.publish_schema10(encoded, selection, build_scorecard=render,
                                                  published_run_scorecard=capture))
        render.assert_not_called()
        self.assertEqual("unchanged", again["state"], again)
        self.assertEqual(transaction, again["scorecard"]["transaction_commit"])
        self.assertIn(f"SCORECARD ALREADY PUBLISHED by run {run_id}: published at {transaction} from rows "
                      f"through {series_commit}", stderr)
        # Reused only because the commits between the rows it names and the
        # transaction's parent are appends.
        writer, snapshot, portable = seen.pop("writer"), seen.pop("snapshot"), seen.pop("portable")
        with self.schema10_doubles(encoded, selection), \
                mock.patch.object(mirror, "_appends_since", return_value=(None, "fixture")):
            outcome, why = mirror.published_run_scorecard(writer, snapshot, portable=portable, **seen)
        self.assertIsNone(outcome)
        self.assertIn(f"sits on {racers[0]}, which is not {series_commit} plus appended rows and other "
                      "runs' proofs", why)

    def test_retry_renders_when_a_row_of_this_run_published_after_its_transaction(self):
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        self.append("portable-cell")
        first, _ = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["scorecard"]["state"], first)
        self.append_naming("late-cell-of-this-run", selection["row"]["run_id"])
        render = mock.Mock(side_effect=mirror.MirrorRefused("rendered again"))
        again, stderr = self.publish_schema10(encoded, selection, build_scorecard=render)
        render.assert_called_once()
        self.assertEqual("derived-failed", again["state"], again)
        self.assertIn("rendered again", again["scorecard"]["error"])
        self.assertIn("rows mentioning this run were added after", stderr)
        self.assertNotIn("SCORECARD ALREADY PUBLISHED", stderr)

    def test_retry_renders_when_the_scorecard_tool_changed(self):
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        self.append("portable-cell")
        first, _ = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["scorecard"]["state"], first)
        self.set_tool("(p/'SCORECARD.md').write_text('another tool\\n')\n")
        render = mock.Mock(side_effect=mirror.MirrorRefused("rendered again"))
        again, stderr = self.publish_schema10(encoded, selection, build_scorecard=render)
        render.assert_called_once()
        self.assertIn("does not bind this measured commit, tool, carrier and rows", stderr)

    def test_retry_check_refuses_every_unproven_binding(self):
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        self.append("portable-cell")
        first, _ = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["scorecard"]["state"], first)
        transaction = first["commit"]
        # Another run's row moves HEAD past the transaction, so the check
        # recomputes the projection of the transaction's own parent.
        self.append("another-runs-cell")
        seen, again, _ = self.captured_check(
            lambda capture: self.publish_schema10(encoded, selection, published_run_scorecard=capture))
        self.assertEqual(transaction, again["scorecard"]["transaction_commit"])
        real = mirror.published_run_scorecard
        writer, snapshot, portable = seen.pop("writer"), seen.pop("snapshot"), seen.pop("portable")
        self.assertNotEqual(transaction, snapshot["source"]["commit"])
        self.assertEqual({"parent", "source_checkout", "target", "finalized_row", "finalized_run_id",
                          "tool_checkout", "results", "net_wrapper"}, set(seen))
        tool_commit = git(self.tool, "rev-parse", "HEAD")

        def check(candidate=None, rows=None, **changes):
            return real(writer, snapshot if rows is None else rows,
                        portable=portable if candidate is None else candidate, **{**seen, **changes})

        def changed(value, path, new):
            copied = copy.deepcopy(value)
            # Published rows are held as text; a mutable list stands in for an edited projection.
            if isinstance(copied, dict) and isinstance(copied.get("rows"), mirror._PublishedRows):
                copied["rows"] = list(copied["rows"])
            holder = copied
            *parents, last = path
            for key in parents:
                holder = holder[key]
            if new is None:
                holder.pop(last)
            else:
                holder[last] = new
            return copied

        actual_snapshot = mirror._published_snapshot

        def projection(path, new):
            # The check recomputes the transaction parent's projection by commit.
            def published(repo, ref="HEAD"):
                value = actual_snapshot(repo, ref)
                return value if ref == "HEAD" else changed(value, path, new)
            return mock.patch.object(mirror, "_published_snapshot", side_effect=published)

        base = git(writer, "rev-parse", transaction + "^")
        self.assertEqual(first["series_commit"], base)

        own_row = next(index for index, value in enumerate(snapshot["rows"]) if value["event_id"] == "portable-cell")
        stored = mirror.scorecard_cells.read_commit(writer, transaction)
        with self.schema10_doubles(encoded, selection):
            outcome, why = check()
            self.assertEqual(transaction, outcome["transaction_commit"], why)
            refusals = {
                "another measured commit": ({"target": "e" * 40}, "is not this run at this measured commit"),
                "another run": ({"finalized_run_id": "another-run"}, "is not this run at this measured commit"),
                "another parent event": ({"candidate": changed(portable, ("selection", "parent", "commit"), "5" * 40)},
                                         "names another parent event"),
                "other operands": ({"candidate": changed(portable, ("artifacts", "schema"), 99)},
                                   "binds other operands"),
                "a row of the transaction's parent changed at HEAD": (
                    {"rows": changed(snapshot, ("rows", own_row, "series", "outcome"), "failed")},
                    "were changed or removed by"),
                "a row of the transaction's parent removed at HEAD": (
                    {"rows": changed(snapshot, ("rows", own_row), None)}, "were changed or removed by"),
                "another producer row": ({}, "the canonical finalized producer changed after preparation"),
            }
            for name, (changes, reason) in refusals.items():
                with self.subTest(name), contextlib.ExitStack() as patches:
                    if name == "another producer row":
                        patches.enter_context(mock.patch.object(mirror, "finalized_producer_row",
                                                                return_value=encoded + b" "))
                    outcome, why = check(**changes)
                    self.assertIsNone(outcome, why)
                    self.assertIn(reason, why)
            for name, path, new in (("rows of another digest", ("rows_sha256",), "0" * 64),
                                    ("another row count", ("rows_read",), 99),
                                    ("another series tree", ("source", "tree"), "0" * 40)):
                with self.subTest(name), projection(path, new):
                    outcome, why = check()
                    self.assertIsNone(outcome, why)
                    self.assertIn(f"metadata differs from the rows of {base}", why)
            # derived_commit_base reads the stored history against its digest
            # first; with that classification held, the check's own read of
            # the bytes it parses refuses as well.
            classified = mirror.derived_commit_base(writer, transaction)
            self.assertEqual((base, mirror.SCORECARD_COMMIT_MESSAGE), classified[:2])
            for name, value in (("a history of other bytes", stored + b" "), ("no stored history", None)):
                for held, reason in ((False, "is not a complete scorecard transaction"),
                                     (True, "stores no history with its recorded digest")):
                    with self.subTest(name, classification_held=held), contextlib.ExitStack() as patches:
                        patches.enter_context(mock.patch.object(mirror.scorecard_cells, "read_commit",
                                                                return_value=value))
                        if held:
                            patches.enter_context(mock.patch.object(mirror, "derived_commit_base",
                                                                    return_value=classified))
                        outcome, why = check()
                        self.assertIsNone(outcome, why)
                        self.assertIn(reason, why)
            with self.subTest("another tool commit"), \
                    mock.patch.object(mirror, "_committed_tool_source", return_value="9" * 40):
                outcome, why = check()
                self.assertIsNone(outcome)
                self.assertIn("does not bind", why)
            with self.subTest("the tool moved during the check"), \
                    mock.patch.object(mirror, "_committed_tool_source", side_effect=[tool_commit, "9" * 40]):
                outcome, why = check()
                self.assertIsNone(outcome)
                self.assertIn("the measured source or the scorecard tool changed during the check", why)
            with self.subTest("the measured source moved during the check"):
                moved = self.source_commit("moved", "2026-09-21T13:00:00Z", base=self.target)
                self.assertNotEqual(self.target, moved)
                try:
                    outcome, why = check()
                finally:
                    git(self.source, "checkout", "-q", "--detach", self.target)
                self.assertIsNone(outcome)
                self.assertIn("the measured source or the scorecard tool changed during the check", why)
            with self.subTest("the canonical terminal was corrected on parent main"), \
                    mock.patch.object(mirror, "_select_published_terminal", return_value=(
                        mock.MagicMock(), changed(selection, ("terminal", "event_sha256"), "5" * 64))):
                outcome, why = check()
                self.assertIsNone(outcome)
                self.assertIn("the canonical terminal changed after preparation", why)
            namespace = mock.MagicMock()
            namespace.verify_origin.side_effect = mirror.finalized_proof.ProofRefused("origin differs")
            with self.subTest("the carrier's origin does not verify"), \
                    mock.patch.object(mirror, "_select_published_terminal", return_value=(namespace, selection)), \
                    self.assertRaisesRegex(mirror.finalized_proof.ProofRefused, "origin differs"):
                check()
            with self.subTest("an operand cannot be read at HEAD"), \
                    mock.patch.object(mirror.finalized_proof, "read_artifact_objects",
                                      side_effect=mirror.finalized_proof.ProofRefused("object missing")), \
                    self.assertRaisesRegex(mirror.finalized_proof.ProofRefused, "object missing"):
                check()
            with self.subTest("carrier added by a proof-only commit"), \
                    mock.patch.object(mirror, "derived_commit_base",
                                      return_value=(snapshot["source"]["commit"],
                                                    mirror.PROOF_FALLBACK_COMMIT_MESSAGE, 2)):
                outcome, why = check()
                self.assertIsNone(outcome)
                self.assertIn("is not a complete scorecard transaction", why)
            with self.subTest("unclassifiable carrier commit"), \
                    mock.patch.object(mirror, "derived_commit_base", return_value=None):
                self.assertIsNone(check()[0])
            with self.subTest("appended rows cannot be read"), \
                    mock.patch.object(mirror, "_rows_naming_run_since", return_value=None):
                outcome, why = check()
                self.assertIsNone(outcome)
                self.assertIn("cannot be read as appended JSON Lines rows", why)

    def test_retry_check_requires_this_hosts_exact_completion_receipt(self):
        # The ledger's bytes cannot show that a render completed; only this
        # host's receipt, naming the transaction exactly, can.
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        run_id = selection["row"]["run_id"]
        self.append("portable-cell")
        first, _ = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["scorecard"]["state"], first)
        transaction = first["commit"]
        seen, again, _ = self.captured_check(
            lambda capture: self.publish_schema10(encoded, selection, published_run_scorecard=capture))
        self.assertEqual(transaction, again["scorecard"]["transaction_commit"])
        real = mirror.published_run_scorecard
        writer, snapshot = seen.pop("writer"), seen.pop("snapshot")
        path = self.receipt_path(run_id)
        genuine = path.read_bytes()
        receipt = json.loads(genuine)
        self.assertEqual({"schema", "run_id", "hermit_commit", "tool_hermit_commit", "transaction_commit",
                          "series_commit", "latest_sha256", "finalized_row_sha256", "files"}, set(receipt))
        elsewhere = self.root / "receipt-elsewhere.json"
        elsewhere.write_bytes(genuine)
        names = "does not name scorecard transaction"
        absent = "this host holds no completion receipt of a render of run"

        def store(value):
            if isinstance(value, bytes):
                path.write_bytes(value)
            else:
                path.write_text(json.dumps(value))

        def replace(make):
            if path.is_dir() and not path.is_symlink():
                path.rmdir()
            else:
                path.unlink(missing_ok=True)
            make()

        cases = {}
        for key, value in receipt.items():
            if isinstance(value, dict):
                entry = sorted(value)[0]
                forged = {**value, entry: "0" * 64}
            else:
                forged = "0" * len(value) if value != "0" * len(value) else "1" * len(value)
            cases[f"another {key}"] = (lambda key=key, forged=forged: store({**receipt, key: forged}),
                                       f"{names} {transaction} as it is: {key} differ")
        cases.update({
            "a missing field": (lambda: store({k: v for k, v in receipt.items() if k != "files"}),
                                "files differ"),
            "an extra field": (lambda: store({**receipt, "rendered": True}), "rendered differ"),
            "not an object": (lambda: store([receipt]), "the whole receipt differ"),
            "not JSON": (lambda: store(b"not json\n"), "is not JSON"),
            "larger than its bound": (lambda: store(genuine + b" " * mirror._RECEIPT_MAX_BYTES),
                                      "is larger than"),
            "absent": (lambda: path.unlink(), "does not exist"),
            "a symbolic link to the genuine bytes": (lambda: replace(lambda: path.symlink_to(elsewhere)),
                                                     "cannot be opened as a file"),
            "a directory": (lambda: replace(path.mkdir), "is not a regular file"),
        })
        with self.schema10_doubles(encoded, selection):
            outcome, why = real(writer, snapshot, **seen)
            self.assertEqual(transaction, outcome["transaction_commit"], why)
            for name, (make, reason) in cases.items():
                with self.subTest(name):
                    try:
                        make()
                        outcome, why = real(writer, snapshot, **seen)
                    finally:
                        replace(lambda: path.write_bytes(genuine))
                    self.assertIsNone(outcome, why)
                    self.assertIn(reason, why)
                    if reason.endswith("differ") or name == "absent":
                        self.assertIn(names if reason.endswith("differ") else absent, why)
            # Restored, the genuine receipt proves the transaction again.
            outcome, why = real(writer, snapshot, **seen)
            self.assertEqual(transaction, outcome["transaction_commit"], why)

    def test_a_published_transaction_without_this_hosts_receipt_is_rendered_again(self):
        # A transaction made exactly as this tool makes it, but with no receipt
        # here (another host pushed it, or the receipt could not be written),
        # is not proof that a render of this run completed.
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        run_id = selection["row"]["run_id"]
        self.append("portable-cell")
        unwritable = mock.Mock(side_effect=OSError("receipt directory is read-only"))
        first, stderr = self.publish_schema10(encoded, selection, write_completion_receipt=unwritable)
        unwritable.assert_called_once()
        self.assertEqual("published", first["state"], first)
        self.assertEqual("published", first["scorecard"]["state"], first)
        self.assertEqual(first["commit"], git(self.remote, "rev-parse", "main"))
        self.assertIn(f"SCORECARD COMPLETION RECEIPT NOT WRITTEN: receipt directory is read-only; a retry "
                      f"of run {run_id} renders the scorecard again", stderr)
        self.assertFalse(self.receipt_path(run_id).exists())
        render = mock.Mock(side_effect=mirror.MirrorRefused("rendered again"))
        again, stderr = self.publish_schema10(encoded, selection, build_scorecard=render)
        render.assert_called_once()
        self.assertEqual("derived-failed", again["state"], again)
        self.assertIn(f"this host holds no completion receipt of a render of run {run_id}", stderr)
        self.assertNotIn("SCORECARD ALREADY PUBLISHED", stderr)

    def test_retry_renders_when_an_operand_of_the_carrier_changed_at_head(self):
        # A later commit may change any path under scorecard/ without naming
        # this run; the carrier's operands are read again at HEAD.
        self.use_history_renderer()
        encoded, selection = self.portable_fixture()
        self.append("portable-cell")
        first, _ = self.publish_schema10(encoded, selection)
        self.assertEqual("published", first["scorecard"]["state"], first)
        transaction = first["commit"]
        objects = sorted(path for path in first["scorecard"]["files"]
                         if path.startswith(mirror.finalized_proof.PREFIX + "/objects/"))
        self.assertTrue(objects, first["scorecard"]["files"])
        editor = self.root / "operand-editor"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(editor)], check=True)
        operand = editor / objects[0]

        def push(message):
            git(editor, "add", "-A")
            git(editor, "commit", "-q", "--allow-empty", "-m", message)
            git(editor, "push", "-q", "origin", "HEAD:main")

        cases = {
            "removed": lambda: operand.unlink(),
            "rewritten": lambda: operand.write_bytes(operand.read_bytes() + b"\n"),
            "made executable": lambda: operand.chmod(0o755),
        }
        reuse = mock.Mock(side_effect=AssertionError("the published scorecard was rendered again"))
        for number, (name, edit) in enumerate(cases.items()):
            with self.subTest(name):
                git(editor, "pull", "-q", "--ff-only", "origin", "main")
                edit()
                push(f"operand {name}")
                self.append(f"after-operand-{number}")
                render = mock.Mock(side_effect=mirror.MirrorRefused("rendered again"))
                again, stderr = self.publish_schema10(encoded, selection, build_scorecard=render)
                render.assert_called_once()
                self.assertIn("rendering the scorecard; this run's publication is not complete on main: "
                              "cannot be decided", stderr)
                self.assertNotIn("SCORECARD ALREADY PUBLISHED", stderr)
                # Restored, the operand proves the transaction again.
                git(editor, "pull", "-q", "--ff-only", "origin", "main")
                git(editor, "checkout", transaction, "--", objects[0])
                push(f"operand {name} restored")
                self.append(f"after-restore-{number}")
                restored, stderr = self.publish_schema10(encoded, selection, build_scorecard=reuse)
                self.assertEqual(transaction, restored["scorecard"]["transaction_commit"], stderr)
        reuse.assert_not_called()

    def test_retry_renders_when_the_published_history_is_not_this_runs_projection(self):
        edit = self.use_history_renderer()
        bindings = ["projection", "comparison_attempt_bindings_v1", "bindings"]
        projection = "is not the projection of"
        result = "records no result of this run"
        forgeries = {
            "not JSON": ({"raw": "not json\n"}, "is not JSON"),
            "no projection": ({"set": [[["projection"], None]]}, projection),
            "another source": ({"set": [[["projection", "source"], "parity"]]}, projection),
            "another source repository": ({"set": [[["projection", "source_repository"],
                                                    "https://example.invalid/ledger.git"]]}, projection),
            "another source commit": ({"set": [[["projection", "source_commit"], "0" * 40]]}, projection),
            "another source tree": ({"set": [[["projection", "source_tree"], "0" * 40]]}, projection),
            "another row count": ({"set": [[["projection", "rows_read"], 0]]}, projection),
            "another refresh time": ({"set": [[["projection", "refreshed_at"], "2026-09-21T12:00:01Z"]]},
                                     projection),
            "bindings that are not a list": ({"set": [[bindings, {}]]}, projection),
            "no schema number": ({"set": [[["schema"], "8"]]}, "lacks the schema number"),
            "cells that are not a list": ({"set": [[["cells"], None]]}, "lacks the schema number"),
            "a pre_series_corpus that is not a flag": ({"set": [[["projection", "pre_series_corpus"], None]]},
                                                       "lacks the schema number"),
            "no bindings": ({"set": [[bindings, []]]}, result),
            "a binding of another run": ({"set": [[[*bindings, 0, "run_id"], "another-run"]]}, result),
            "a binding at another measured commit": ({"set": [[[*bindings, 0, "hermit_sha"], "e" * 40]]}, result),
            "a binding from another ledger commit": ({"set": [[[*bindings, 0, "snapshot", "commit"], "0" * 40]]},
                                                     result),
            "a binding from other rows": ({"set": [[[*bindings, 0, "snapshot", "rows_sha256"], "0" * 64]]},
                                          result),
        }
        for number, (name, (spec, reason)) in enumerate(forgeries.items(), 1):
            with self.subTest(name):
                run_id = f"forged-history-run-{number}"

                def fixture():
                    prepared, encoded = self.derived_fixture(run_id=run_id)
                    prepared["selection"]["terminal"]["event_id"] = f"forged-history-terminal-{number}"
                    return prepared, encoded

                edit.write_text(json.dumps(spec))
                prepared, encoded = fixture()
                self.append(f"forged-history-{number}")
                with self.proof_doubles(prepared, encoded):
                    first, _ = self.publish_at(self.target, results=self.root / "results", finalized_run_id=run_id)
                edit.unlink()
                self.assertEqual("published", first["scorecard"]["state"], first)
                retry, encoded = fixture()
                render = mock.Mock(side_effect=mirror.MirrorRefused("rendered again"))
                with self.proof_doubles(retry, encoded), mock.patch.object(mirror, "build_scorecard", render):
                    again, stderr = self.publish_at(self.target, results=self.root / "results",
                                                    finalized_run_id=run_id)
                render.assert_called_once()
                self.assertEqual("derived-failed", again["state"], again)
                self.assertIn(reason, stderr)
                self.assertNotIn("SCORECARD ALREADY PUBLISHED", stderr)

    def test_retry_after_a_failed_scorecard_renders_again(self):
        # The first attempt's renderer failed, so only the derived carrier
        # published, under the proof-only message; that is no scorecard.
        self.set_tool("sys.exit('renderer exploded')\n")
        prepared, encoded = self.derived_fixture()
        self.append("proof-without-scorecard")
        with self.proof_doubles(prepared, encoded):
            first, _ = self.publish_at(self.target, results=self.root / "results",
                                       finalized_run_id=prepared["selection"]["row"]["run_id"])
        self.assertEqual("derived-failed", first["state"], first)
        self.assertEqual("published", first["portable_proof"]["state"], first)
        self.assertFalse(self.receipt_path(prepared["selection"]["row"]["run_id"]).exists())
        retry, encoded = self.derived_fixture()
        render = mock.Mock(side_effect=mirror.MirrorRefused("rendered again"))
        with self.proof_doubles(retry, encoded), mock.patch.object(mirror, "build_scorecard", render):
            again, stderr = self.publish_at(self.target, results=self.root / "results",
                                            finalized_run_id=retry["selection"]["row"]["run_id"])
        render.assert_called_once()
        self.assertEqual("derived-failed", again["state"], again)
        self.assertIn("is not a complete scorecard transaction", stderr)

    def test_retry_of_a_published_derived_run_reuses_its_scorecard_transaction(self):
        self.use_history_renderer()
        prepared, encoded = self.derived_fixture()
        run_id = prepared["selection"]["row"]["run_id"]
        self.append("derived-cell")
        with self.proof_doubles(prepared, encoded):
            first, _ = self.publish_at(self.target, results=self.root / "results", finalized_run_id=run_id)
        self.assertEqual("published", first["scorecard"]["state"], first)
        retry, encoded = self.derived_fixture()
        render = mock.Mock(side_effect=AssertionError("the published scorecard was rendered again"))

        def publish(capture):
            with self.proof_doubles(retry, encoded), mock.patch.object(mirror, "build_scorecard", render), \
                    mock.patch.object(mirror, "published_run_scorecard", capture):
                return self.publish_at(self.target, results=self.root / "results", finalized_run_id=run_id)

        seen, again, stderr = self.captured_check(publish)
        render.assert_not_called()
        self.assertEqual(first["commit"], again["scorecard"]["transaction_commit"])
        self.assertEqual(first["scorecard"]["files"], again["scorecard"]["files"])
        self.assertEqual(first["commit"], git(self.remote, "rev-parse", "main"))
        self.assertIn("SCORECARD ALREADY PUBLISHED", stderr)
        # The derived operands are emitted again from the raw results, as a
        # retry's carrier staging requires, and must be the published ones.
        writer, snapshot = seen.pop("writer"), seen.pop("snapshot")
        real = mirror.published_run_scorecard
        with self.subTest("without the raw results"):
            again, encoded_again = self.derived_fixture()
            with self.proof_doubles(again, encoded_again):
                outcome, why = real(writer, snapshot, **{**seen, "portable": again, "results": None})
            self.assertIsNone(outcome)
            self.assertIn("cannot be checked without the run's raw results", why)
        with self.subTest("raw results that emit other operands"):
            again, encoded_again = self.derived_fixture()
            value = json.loads(encoded_again)
            value["attempts"][0]["source_row_sha256"] = "c" * 64
            other = mirror.finalized_proof.canonical_bytes(value) + b"\n"
            with self.proof_doubles(again, other):
                outcome, why = real(writer, snapshot, **{**seen, "portable": again})
            self.assertIsNone(outcome)
            self.assertIn("binds other operands than this run's raw results", why)
        with self.subTest("the same raw results"):
            again, encoded_again = self.derived_fixture()
            with self.proof_doubles(again, encoded_again):
                outcome, why = real(writer, snapshot, **{**seen, "portable": again})
            self.assertEqual(first["commit"], outcome["transaction_commit"], why)

    def test_skip_lines_of_other_renders_are_relayed_unchanged(self):
        # Only project-and-observe-results' skipped snapshot rows are folded.
        line = "  skipped s.json:rows[0]:series-1: a/b/verify/ptrace from validate run r1 repetition 1 @ " \
               + "d" * 40 + ": no exact test/mode/backend match in the tracked manifest"
        self.set_tool(f"print({line!r})\nprint({line!r})\n(p/'SCORECARD.md').write_text('rendered\\n')\n")
        self.append("unfinalized-cell")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            report = self.publish()
        self.assertEqual("published", report["scorecard"]["state"], report)
        self.assertEqual(2, stderr.getvalue().count(line + "\n"))
        self.assertNotIn("row(s) of snapshot", stderr.getvalue())
        # No finalized run, so no completion receipt.
        self.assertFalse((self.parent / mirror.RECEIPTS_RELATIVE).exists())


class SelectPublishedTerminalReuseTest(unittest.TestCase):
    """The recheck before a carrier is staged selects from an unmoved main without rebuilding.

    A carrier attachment selected its terminal from the parent's main twice,
    once preparing and once just before staging, and built the whole
    committed validation namespace both times: 47.7 s of 195.5 s each, on
    2026-10-04 (https://github.com/rrnewton/dev-hermit/issues/394).
    """

    ROW = {"repo": "hermit", "run_id": "validate-reuse", "record_id": None, "commit": "c" * 40,
           "cwd": "/checkout", "started_at": "2026-10-04T00:00:00Z", "schema_version": 10}

    def namespace(self, commit: str):
        import validate_rows
        built = object.__new__(validate_rows.CommittedValidationNamespace)
        built._commit = commit
        built.select = lambda **selector: {"row": copy.deepcopy(self.ROW), "terminal": {"run_id": selector["run_id"]},
                                           "parent": {"commit": commit}}
        return built

    def select(self, main: str, reuse, rebuilt=None):
        import validate_rows
        cache = {"namespace": reuse}
        with mock.patch.object(mirror, "_network") as network, \
                mock.patch.object(mirror, "_git", return_value=main), \
                mock.patch.object(validate_rows, "capture_committed_validation_namespace",
                                  return_value=rebuilt) as capture:
            namespace, selected = mirror._select_published_terminal(
                Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env", cache=cache)
        network.assert_called_once()
        # The namespace this selection used is the one the next selection may reuse.
        self.assertIs(namespace, cache["namespace"])
        return namespace, selected, capture

    def test_an_unmoved_main_selects_from_the_earlier_namespace(self):
        earlier = self.namespace("a" * 40)
        namespace, selected, capture = self.select("a" * 40, earlier)
        capture.assert_not_called()
        self.assertIs(earlier, namespace)
        self.assertEqual(self.ROW, selected["row"])

    def test_a_moved_main_rebuilds_the_namespace(self):
        rebuilt = self.namespace("b" * 40)
        namespace, selected, capture = self.select("b" * 40, self.namespace("a" * 40), rebuilt)
        capture.assert_called_once_with(Path("/publisher"), "b" * 40, state_root=Path("/parent"))
        self.assertIs(rebuilt, namespace)
        self.assertEqual({"commit": "b" * 40}, selected["parent"])

    def test_without_an_earlier_namespace_it_is_built(self):
        rebuilt = self.namespace("a" * 40)
        for reuse in (None, mock.MagicMock(commit="a" * 40)):
            with self.subTest(reuse=reuse):
                namespace, _, capture = self.select("a" * 40, reuse, rebuilt)
                capture.assert_called_once()
                self.assertIs(rebuilt, namespace)

    def test_a_stale_namespace_is_released_before_its_replacement_is_built(self):
        # A namespace holds the whole canonical validation ledger (about 5 GB on
        # 2026-10-11); kept beside its replacement it set an 18.8 GB peak
        # (https://github.com/rrnewton/dev-hermit/issues/554).
        import validate_rows
        cache = {"namespace": self.namespace("a" * 40)}
        stale = weakref.ref(cache["namespace"])
        rebuilt = self.namespace("b" * 40)
        alive_during_rebuild = []

        def capture(*_args, **_kwargs):
            gc.collect()
            alive_during_rebuild.append(stale() is not None)
            return rebuilt

        with mock.patch.object(mirror, "_network"), mock.patch.object(mirror, "_git", return_value="b" * 40), \
                mock.patch.object(validate_rows, "capture_committed_validation_namespace", side_effect=capture):
            namespace, _ = mirror._select_published_terminal(
                Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env", cache=cache)
        self.assertEqual([False], alive_during_rebuild)
        self.assertIs(rebuilt, namespace)
        self.assertIs(rebuilt, cache["namespace"])

    def test_a_changed_terminal_is_still_refused(self):
        earlier = self.namespace("a" * 40)
        earlier.select = lambda **selector: {"row": dict(self.ROW, commit="d" * 40),
                                             "terminal": {}, "parent": {}}
        with self.assertRaisesRegex(mirror.MirrorRefused, "differs from the finalized producer"):
            self.select("a" * 40, earlier)

    def seed(self, root: str = "/publisher", state: str = "/parent", advanced=None):
        import validate_rows
        seed = object.__new__(validate_rows.CommittedNamespaceSeed)
        seed._root, seed._state = Path(root).resolve(), Path(state).resolve()
        seed.advanced_to = mock.MagicMock(return_value=advanced)
        return seed

    def test_a_moved_main_advances_the_earlier_namespace_after_releasing_it(self):
        # Main moves every few minutes, so the recheck rebuilt the namespace in
        # nearly every publication: 321 s and a 5.7 GB peak at dev-hermit
        # 5bd40dc6. It is advanced from the earlier build's seed instead, and
        # the stale namespace is still released first.
        import validate_rows
        advanced = self.namespace("b" * 40)
        seed = self.seed(advanced=advanced)
        cache = {"namespace": self.namespace("a" * 40)}
        cache["namespace"].advance_seed = lambda: seed
        stale = weakref.ref(cache["namespace"])
        alive_while_advancing = []

        def advance(commit):
            gc.collect()
            alive_while_advancing.append(stale() is not None)
            return advanced

        seed.advanced_to.side_effect = advance
        with mock.patch.object(mirror, "_network"), mock.patch.object(mirror, "_git", return_value="b" * 40), \
                mock.patch.object(validate_rows, "capture_committed_validation_namespace") as capture:
            namespace, selected = mirror._select_published_terminal(
                Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env", cache=cache)
        self.assertEqual({"namespace": advanced}, cache)
        capture.assert_not_called()
        seed.advanced_to.assert_called_once_with("b" * 40)
        self.assertEqual([False], alive_while_advancing)
        self.assertIs(advanced, namespace)
        self.assertEqual({"commit": "b" * 40}, selected["parent"])

    def test_the_preparations_seed_is_advanced_and_consumed(self):
        import validate_rows
        advanced = self.namespace("b" * 40)
        seed = self.seed(advanced=advanced)
        cache = {"namespace_seed": seed}
        with mock.patch.object(mirror, "_network"), mock.patch.object(mirror, "_git", return_value="b" * 40), \
                mock.patch.object(validate_rows, "capture_committed_validation_namespace") as capture:
            namespace, _ = mirror._select_published_terminal(
                Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env", cache=cache)
        capture.assert_not_called()
        seed.advanced_to.assert_called_once_with("b" * 40)
        self.assertIs(advanced, namespace)
        self.assertEqual({"namespace": advanced}, cache)

    def advance_with_record(self, outcome: str, reason=None):
        # A seed whose advance records its outcome as the real one does.
        import validate_rows
        advanced = self.namespace("b" * 40)
        seed = self.seed(advanced=advanced)

        def advance(commit):
            validate_rows._note_advance("a" * 40, commit, outcome, reason, 1.5)
            return advanced

        seed.advanced_to.side_effect = advance
        stderr = io.StringIO()
        with mock.patch.object(mirror, "_network"), mock.patch.object(mirror, "_git", return_value="b" * 40), \
                mock.patch.object(validate_rows, "capture_committed_validation_namespace") as capture, \
                contextlib.redirect_stderr(stderr):
            namespace, _ = mirror._select_published_terminal(
                Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env",
                cache={"namespace_seed": seed})
        capture.assert_not_called()
        self.assertIs(advanced, namespace)
        return stderr.getvalue()

    def test_each_advance_outcome_is_reported_on_stderr(self):
        # NAMESPACE_ADVANCES lives only in the process; a seed that always fell
        # back would otherwise cost a full build per publication unseen.
        self.assertEqual(
            "ledger publication: committed namespace advanced aaaaaaaaaaaa -> bbbbbbbbbbbb in 1.5 s\n",
            self.advance_with_record("advanced"))
        self.assertEqual(
            "ledger publication: committed namespace advance aaaaaaaaaaaa -> bbbbbbbbbbbb refused after "
            "1.5 s, built in full: _AdvanceRefused: the earlier commit is not an ancestor\n",
            self.advance_with_record("rebuilt", "_AdvanceRefused: the earlier commit is not an ancestor"))

    def test_a_namespace_that_cannot_make_a_seed_is_rebuilt_not_refused(self):
        import validate_rows
        cache = {"namespace": self.namespace("a" * 40)}

        def broken():
            raise RuntimeError("seed construction failed")

        cache["namespace"].advance_seed = broken
        rebuilt = self.namespace("b" * 40)
        stderr = io.StringIO()
        with mock.patch.object(mirror, "_network"), mock.patch.object(mirror, "_git", return_value="b" * 40), \
                mock.patch.object(validate_rows, "capture_committed_validation_namespace",
                                  return_value=rebuilt) as capture, contextlib.redirect_stderr(stderr):
            namespace, _ = mirror._select_published_terminal(
                Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env", cache=cache)
        capture.assert_called_once_with(Path("/publisher"), "b" * 40, state_root=Path("/parent"))
        self.assertIs(rebuilt, namespace)
        self.assertIn("seed unavailable, the recheck builds in full: RuntimeError: seed construction failed",
                      stderr.getvalue())
        # The preparation's seed is made the same way: a failure is no seed.
        broken_namespace = self.namespace("a" * 40)
        broken_namespace.advance_seed = broken
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertIsNone(mirror._namespace_seed(broken_namespace))
        self.assertIsNone(mirror._namespace_seed(mock.MagicMock()))

    def test_a_seed_of_another_source_or_state_is_not_advanced(self):
        import validate_rows
        for seed in (self.seed(root="/elsewhere"), self.seed(state="/elsewhere")):
            with self.subTest(root=seed.root, state=seed.state_root):
                rebuilt = self.namespace("b" * 40)
                cache = {"namespace_seed": seed}
                with mock.patch.object(mirror, "_network"), \
                        mock.patch.object(mirror, "_git", return_value="b" * 40), \
                        mock.patch.object(validate_rows, "capture_committed_validation_namespace",
                                          return_value=rebuilt) as capture:
                    namespace, _ = mirror._select_published_terminal(
                        Path("/publisher"), Path("/parent"), self.ROW, net_wrapper="/usr/bin/env",
                        cache=cache)
                capture.assert_called_once_with(Path("/publisher"), "b" * 40, state_root=Path("/parent"))
                seed.advanced_to.assert_not_called()
                self.assertIs(rebuilt, namespace)


class SelectPublishedTerminalAdvanceTest(unittest.TestCase):
    """The second selection of a publication advances the first one's namespace, on real Git."""

    def setUp(self):
        import validate_rows
        self.validate_rows = validate_rows
        self.tmp = Path(tempfile.mkdtemp(prefix="mirror-namespace-advance-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.source = self.tmp / "publisher"
        self.parent = self.tmp / "parent"
        self.parent.mkdir()
        self.source.mkdir()
        git(self.source, "init", "-q")
        git(self.source, "config", "user.email", "mirror-test@example.invalid")
        git(self.source, "config", "user.name", "mirror test")
        git(self.source, "remote", "add", "origin", validate_rows.CANONICAL_PARENT_REPOSITORY)
        self.shard = self.source / "ledger/hermit/hosta/2026-08.jsonl"
        self.shard.parent.mkdir(parents=True)

    def row(self, run_id: str, finished_at: str) -> dict:
        return {"schema_version": 6, "repo": "hermit", "run_id": run_id, "record_id": f"{run_id}-final",
                "producer": "hermit-validate-rs", "host": "hosta", "commit": "a" * 40,
                "cwd": f"{self.validate_rows.WORKSPACE_TOKEN}/hermit",
                "started_at": "2026-08-09T01:00:00Z", "finished_at": finished_at, "result": "fail"}

    def append(self, row: dict) -> str:
        event = self.validate_rows._event_for_row(row, self.source)
        with self.shard.open("a") as out:
            out.write(json.dumps(event, sort_keys=True) + "\n")
        git(self.source, "add", "ledger")
        git(self.source, "commit", "-qm", "ledger: append 1 event(s)")
        commit = git(self.source, "rev-parse", "HEAD")
        git(self.source, "update-ref", "refs/remotes/origin/main", commit)
        return commit

    def test_the_recheck_advances_the_preparations_namespace(self):
        validate_rows = self.validate_rows
        row = self.row("advance-run", "2026-08-09T01:01:00Z")
        first = self.append(row)
        selected_row = {**row, "cwd": str(self.parent.resolve() / "hermit")}
        with mock.patch.object(mirror, "_network"):
            # The preparation selects, keeps only the seed, and drops the namespace.
            namespace, selected = mirror._select_published_terminal(
                self.source, self.parent, selected_row, net_wrapper="/usr/bin/env")
            self.assertEqual(first, namespace.commit)
            prepared = {"namespace_seed": namespace.advance_seed()}
            del namespace
            # Main moves (another run's row lands) before the recheck.
            second = self.append(self.row("another-run", "2026-08-09T02:00:00Z"))
            validate_rows.NAMESPACE_ADVANCES.clear()
            stderr = io.StringIO()
            with mock.patch.object(validate_rows, "capture_committed_validation_namespace",
                                   wraps=validate_rows.capture_committed_validation_namespace) as capture, \
                    contextlib.redirect_stderr(stderr):
                namespace, current = mirror._select_published_terminal(
                    self.source, self.parent, selected_row, net_wrapper="/usr/bin/env", cache=prepared)
        capture.assert_not_called()
        self.assertRegex(stderr.getvalue(), rf"^ledger publication: committed namespace advanced "
                                            rf"{first[:12]} -> {second[:12]} in [0-9.]+ s\n$")
        self.assertEqual([("advanced", first, second)],
                         [(item["outcome"], item["from"], item["to"]) for item in validate_rows.NAMESPACE_ADVANCES])
        self.assertEqual({"namespace": namespace}, prepared)
        self.assertEqual(second, namespace.commit)
        self.assertEqual(selected["terminal"], current["terminal"])
        self.assertEqual(second, current["parent"]["commit"])
        namespace.verify_origin(selected["parent"], selected["terminal"])
        # The advanced namespace answers as a full build at the same commit.
        full = validate_rows.capture_committed_validation_namespace(self.source, second, state_root=self.parent)
        self.assertEqual(full.select(**{key: selected_row[key] for key in (
            "repo", "run_id", "record_id", "commit", "cwd", "started_at")}), current)
        self.assertEqual(full.rows(), namespace.rows())
        self.assertEqual(full.events(), namespace.events())


class PublishedRowsTest(unittest.TestCase):
    """A snapshot's rows held as canonical text keep every byte the publication writes.

    Every series row as Python objects was about 7 GB on 2026-10-11, held
    through the scorecard render (https://github.com/rrnewton/dev-hermit/issues/554).
    """

    ROWS = [
        {"event_id": "e-1", "emitted_at": "2026-10-11T01:00:00Z", "z": [1, 2.5, None, True],
         "series": {"cell": "c\u00e9ll", "nested": {"b": 1, "a": "\u2603"}}},
        {"event_id": "e-2", "emitted_at": "2026-10-11T03:00:00Z", "a": "plain", "series": {}},
        {"event_id": "e-3", "emitted_at": "2026-10-11T02:00:00Z", "q": "quote\"and\\slash"},
    ]

    def snapshot(self, rows):
        return {"schema": "fixture-schema", "rows_read": len(rows),
                "rows_sha256": hashlib.sha256(mirror.series._canonical_rows_bytes(self.ROWS)).hexdigest(),
                "source": {"commit": "a" * 40, "tree": "b" * 40, "path": "series", "\u00fc": 1},
                "rows": rows}

    def test_the_snapshot_file_and_row_digest_are_byte_identical(self):
        held = mirror._PublishedRows(self.ROWS)
        with tempfile.TemporaryDirectory() as directory:
            for rows in (held, list(self.ROWS)):
                path = Path(directory) / "snapshot.json"
                digest = mirror._write_snapshot(self.snapshot(rows), path)
                expected = mirror.series._snapshot_bytes(self.snapshot(list(self.ROWS)))
                self.assertEqual(expected, path.read_bytes())
                self.assertEqual(hashlib.sha256(expected).hexdigest(), digest)
        self.assertEqual(hashlib.sha256(mirror.series._canonical_rows_bytes(self.ROWS)).hexdigest(), held.digest())

    def test_rows_read_back_as_the_original_values(self):
        held = mirror._PublishedRows(self.ROWS)
        self.assertEqual(3, len(held))
        self.assertEqual(self.ROWS, list(held))
        self.assertEqual(self.ROWS[1], held[1])
        self.assertEqual(self.ROWS[1:], held[1:])
        self.assertEqual("2026-10-11T03:00:00Z", mirror._max_emitted_at(held))
        self.assertEqual(mirror._max_emitted_at(self.ROWS), mirror._max_emitted_at(held))
        self.assertIsNone(mirror._max_emitted_at(mirror._PublishedRows([])))

    def test_two_snapshots_compare_as_the_canonical_rows_did(self):
        def old_changed(current, projected):
            rows = {row["event_id"]: mirror.series._canonical_row_json(row) for row in current}
            return any(rows.get(row["event_id"]) != mirror.series._canonical_row_json(row) for row in projected)

        def new_changed(current, projected):
            rows = mirror._rows_by_event_id(current)
            return any(rows.get(key) != text for key, text in mirror._rows_by_event_id(projected).items())

        edited = copy.deepcopy(self.ROWS)
        edited[0]["series"]["cell"] = "c\u00e8ll"
        cases = [(self.ROWS, self.ROWS[:2]), (self.ROWS[1:], self.ROWS), (edited, self.ROWS), (self.ROWS, self.ROWS)]
        for current, projected in cases:
            with self.subTest(current=len(current), projected=len(projected)):
                expected = old_changed(current, projected)
                self.assertEqual(expected, new_changed(mirror._PublishedRows(current), mirror._PublishedRows(projected)))
                self.assertEqual(expected, new_changed(current, projected))

    def test_rows_held_as_text_keep_no_object_per_row(self):
        # A str per row, kept while the rows it was made from were freed,
        # pinned 8.1 GB of RSS for 1.3 GB of text on the real ledger.
        rows = [{"event_id": f"e-{index}", "emitted_at": f"2026-10-11T00:00:{index % 60:02d}Z", "v": index}
                for index in range(20000)]
        held = mirror._PublishedRows(rows)
        del rows
        gc.collect()
        with_rows = sys.getallocatedblocks()
        del held
        gc.collect()
        self.assertLess(with_rows - sys.getallocatedblocks(), 100)

    def test_rows_held_as_text_take_a_fraction_of_their_objects(self):
        rows = [{"event_id": f"e-{index}", "emitted_at": "2026-10-11T00:00:00Z",
                 "series": {"cell": f"cell-{index % 97}", "producer": "fixture", "run_id": f"run-{index}",
                            "run_index": index % 5, "tree": "t" * 40, "outcome": "passed",
                            "metrics": {"wall_ms": index, "cpu_ms": index * 2}}}
                for index in range(20000)]
        tracemalloc.start()
        try:
            before = tracemalloc.get_traced_memory()[0]
            objects = copy.deepcopy(rows)
            as_objects = tracemalloc.get_traced_memory()[0] - before
            del objects
            before = tracemalloc.get_traced_memory()[0]
            held = mirror._PublishedRows(rows)
            as_text = tracemalloc.get_traced_memory()[0] - before
        finally:
            tracemalloc.stop()
        self.assertEqual(20000, len(held))
        self.assertLess(as_text * 2, as_objects, (as_text, as_objects))


class RowsNamingRunSinceTest(unittest.TestCase):
    """Which ledger changes after a scorecard transaction prove the run unchanged."""

    RUN = "validate-ops-tick-fixture"

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="rows-since-test-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name) / "ledger"
        config = Path(self.temporary.name) / "gitconfig"
        config.write_text("[user]\n name = Fixture\n email = fixture@example.invalid\n"
                          "[init]\n defaultBranch = main\n")
        self.enterContext(mock.patch.dict(os.environ, {"GIT_CONFIG_GLOBAL": str(config),
                                                       "GIT_CONFIG_NOSYSTEM": "1"}))
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        self.shard = self.repo / "series/hermit/fixture/2026-10.jsonl"
        self.shard.parent.mkdir(parents=True)
        self.shard.write_text(json.dumps(row("before")) + "\n")
        (self.repo / "latest.json").write_text("{}\n")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "base")
        self.base = git(self.repo, "rev-parse", "HEAD")

    def change(self, edit) -> str:
        git(self.repo, "checkout", "-q", "--detach", self.base)
        edit()
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-qm", "change")
        return git(self.repo, "rev-parse", "HEAD")

    def since(self, head: str):
        return mirror._rows_naming_run_since(self.repo, self.base, head, self.RUN)

    def appended(self, *values, path: Path | None = None):
        def edit():
            target = path or self.shard
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("a") as stream:
                for value in values:
                    stream.write((value if isinstance(value, str) else json.dumps(value)) + "\n")
        return edit

    def test_appends_are_read_and_anything_else_cannot_be_decided(self):
        own = row("late")
        own["run_id"] = self.RUN
        nested = row("nested")
        nested["series"]["note"] = f"retained from {self.RUN}"

        def scorecard_outputs():
            (self.repo / "latest.json").write_text('{"moved": true}\n')
            (self.repo / "scorecard/finalized/runs").mkdir(parents=True)
            (self.repo / "scorecard/finalized/runs/other.json").write_text("{}\n")

        def both():
            scorecard_outputs()
            self.appended(row("another-run"))()

        def rewrite():
            self.shard.write_text(json.dumps(row("before", "failed")) + "\n" + json.dumps(row("x")) + "\n")

        def unterminated():
            self.shard.write_bytes(self.shard.read_bytes().rstrip(b"\n"))
            git(self.repo, "add", "-A")
            git(self.repo, "commit", "-qm", "unterminated")
            self.base = git(self.repo, "rev-parse", "HEAD")
            self.appended(row("another-run"))()

        def executable():
            self.appended(row("another-run"))()
            self.shard.chmod(0o755)

        cases = [
            ("no change", lambda: None, False, True),
            ("another run's rows and scorecard outputs", both, False, False),
            ("a new shard of another run", self.appended(row("x"), path=self.repo / "parity/a/b/2026-10.jsonl"), False, False),
            ("this run's row", self.appended(own), True, False),
            ("this run's row in a new shard", self.appended(own, path=self.repo / "quarantine/x/y.jsonl"), True, False),
            ("this run named inside a row", self.appended(nested), True, False),
            ("a rewritten row", rewrite, None, False),
            ("an append after an unterminated line", unterminated, None, False),
            ("a removed shard", lambda: self.shard.unlink(), None, False),
            ("a mode change", executable, None, False),
            # Valid JSON naming another run: refused for its path, not its bytes.
            ("a row in a file that is not a JSON Lines shard",
             lambda: (self.repo / "README").write_text(json.dumps(row("another-run")) + "\n"), None, False),
            ("an appended array", self.appended("[1]"), None, False),
            ("an appended fragment", self.appended('{"run_id": '), None, False),
        ]
        for name, edit, expected, at_base in cases:
            with self.subTest(name):
                base = self.base
                head = self.base if at_base else self.change(edit)
                self.assertIs(expected, self.since(head))
                self.base = base

    def test_a_new_refused_input_file_is_read_like_a_row(self):
        """A series-quarantine/v3 refusal's input bytes are in their own file.

        The publisher adds them in the same commit as the refusal's row, so a
        completion check after any refusal publication reads them.
        """
        published = b"line from a published run\n"
        published_path = self.repo / series.quarantine_input_path(hashlib.sha256(published).hexdigest())
        published_path.parent.mkdir(parents=True)
        published_path.write_bytes(published)
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-qm", "published input")
        self.base = git(self.repo, "rev-parse", "HEAD")

        def stored(content: bytes, digest: str | None = None, prefix: str | None = None):
            digest = digest or hashlib.sha256(content).hexdigest()
            path = self.repo / series.QUARANTINE_INPUTS_ROOT / (prefix or digest[:2]) / digest
            def edit():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
            return edit

        def refusal(content: bytes):
            def edit():
                self.appended(row("another-run"), path=self.repo / "quarantine/series/a/b.jsonl")()
                stored(content)()
            return edit

        def executable():
            stored(b"another run\n")()
            digest = hashlib.sha256(b"another run\n").hexdigest()
            (self.repo / series.quarantine_input_path(digest)).chmod(0o755)

        other = b"refused bytes of another run\n"
        cases = [
            ("another run's refusal and its input", refusal(other), False),
            ("an input naming this run", refusal(f"rows of {self.RUN}\n".encode()), True),
            ("binary input bytes naming this run", stored(b"\xff\xfe" + self.RUN.encode()), True),
            ("an input file that does not hash to its name", stored(other, digest="0" * 64), None),
            ("an input file under another prefix",
             stored(other, prefix="ff" if hashlib.sha256(other).hexdigest()[:2] != "ff" else "00"), None),
            ("a published input file changed", lambda: published_path.write_bytes(b"other\n"), None),
            ("a published input file removed", lambda: published_path.unlink(), None),
            ("an executable input file", executable, None),
        ]
        for name, edit, expected in cases:
            with self.subTest(name):
                self.assertIs(expected, self.since(self.change(edit)))
        with self.subTest("an input file over the input file ceiling"):
            head = self.change(stored(other))
            with mock.patch.object(series, "QUARANTINE_INPUT_FILE_CEILING_BYTES", len(other) - 1):
                self.assertIsNone(self.since(head))
            self.assertIs(False, self.since(head))


class AppendsSinceTest(unittest.TestCase):
    """Which changes on main a derived commit that lost a push race is committed again over."""

    RUN = {"runs": {"this-run"}, "events": {"this-run-event"}}

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="appends-since-test-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name) / "ledger"
        config = Path(self.temporary.name) / "gitconfig"
        config.write_text("[user]\n name = Fixture\n email = fixture@example.invalid\n"
                          "[init]\n defaultBranch = main\n")
        self.enterContext(mock.patch.dict(os.environ, {"GIT_CONFIG_GLOBAL": str(config),
                                                       "GIT_CONFIG_NOSYSTEM": "1"}))
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        self.shard = "series/hermit/fixture/2026-10.jsonl"
        self.published = DirectLedgerFixture._proof_files("published-run")
        refused = b"published refusal input\n"
        self.refused = series.quarantine_input_path(hashlib.sha256(refused).hexdigest())
        self.write({self.shard: (json.dumps(row("before")) + "\n").encode(), "latest.json": b"{}\n",
                    "scorecard/SCORECARD.md": b"published\n", self.refused: refused, **self.published})
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "base")
        self.base = git(self.repo, "rev-parse", "HEAD")

    def write(self, files: dict[str, bytes]) -> None:
        for path, content in files.items():
            (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / path).write_bytes(content)

    def appended(self, *values, path: str | None = None):
        def edit():
            target = self.repo / (path or self.shard)
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("a") as stream:
                for value in values:
                    stream.write((value if isinstance(value, str) else json.dumps(value)) + "\n")
        return edit

    def change(self, edit) -> str:
        git(self.repo, "checkout", "-q", "--detach", self.base)
        edit()
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-qm", "change")
        return git(self.repo, "rev-parse", "HEAD")

    def appends(self, head: str, **names):
        return mirror._appends_since(self.repo, self.base, head, **names)

    def test_appended_data_is_listed_with_its_mode_and_blob(self):
        refused = b"another run's refusal input\n"
        cases = [
            ("appended rows", self.appended(row("x"), row("y"))),
            ("a new shard", self.appended(row("x"), path="parity/a/b/2026-10.jsonl")),
            ("a new refused input and its row", lambda: (
                self.appended(row("x"), path="quarantine/series/a/b.jsonl")(),
                self.write({series.quarantine_input_path(hashlib.sha256(refused).hexdigest()): refused}))),
            ("another run's proof", lambda: self.write(DirectLedgerFixture._proof_files("other-run"))),
        ]
        self.assertEqual(({}, ""), self.appends(self.base, **self.RUN))
        for name, edit in cases:
            with self.subTest(name):
                head = self.change(edit)
                paths = git(self.repo, "diff-tree", "--no-commit-id", "--name-only", "-r",
                            self.base, head).splitlines()
                self.assertTrue(paths)
                self.assertEqual(({path: ("100644", git(self.repo, "rev-parse", f"{head}:{path}"))
                                   for path in paths}, ""), self.appends(head, **self.RUN))

    def test_anything_else_is_refused_with_the_change_that_is_not_appended_data(self):
        shard = self.shard
        obj = next(path for path in self.published if "/objects/" in path)
        carrier = next(path for path in self.published if "/runs/" in path)
        other = DirectLedgerFixture._proof_files("other-run")
        other_carrier = next(path for path in other if "/runs/" in path)
        refused = b"another run's refusal input\n"
        digest = hashlib.sha256(refused).hexdigest()
        misplaced = f"{series.QUARANTINE_INPUTS_ROOT}/{'ff' if digest[:2] != 'ff' else '00'}/{digest}"

        def rewrite():
            self.write({shard: (json.dumps(row("before", "failed")) + "\n" + json.dumps(row("x")) + "\n").encode()})

        def unterminated():
            self.write({shard: (self.repo / shard).read_bytes().rstrip(b"\n")})
            git(self.repo, "add", "-A")
            git(self.repo, "commit", "-qm", "unterminated")
            self.base = git(self.repo, "rev-parse", "HEAD")
            self.appended(row("x"))()

        def chmod(path: str, edit=lambda: None):
            def change():
                edit()
                (self.repo / path).chmod(0o755)
            return change

        def remove(path: str):
            return lambda: (self.repo / path).unlink()

        cases = [
            ("latest.json", lambda: self.write({"latest.json": b'{"moved": true}\n'}),
             "latest.json (M 100644 -> 100644)"),
            ("another scorecard", lambda: self.write({"scorecard/SCORECARD.md": b"another\n"}),
             "scorecard/SCORECARD.md (M 100644 -> 100644)"),
            ("JSON Lines under scorecard/", self.appended(row("x"), path="scorecard/notes.jsonl"),
             "scorecard/notes.jsonl (A 000000 -> 100644)"),
            ("a rewritten row", rewrite, f"{shard} (M 100644 -> 100644)"),
            ("a truncated shard", lambda: self.write({shard: b""}), f"{shard} (M 100644 -> 100644)"),
            ("an append after an unterminated line", unterminated, f"{shard} (M 100644 -> 100644)"),
            ("a removed shard", remove(shard), f"{shard} (D 100644 -> 000000)"),
            ("a mode change", chmod(shard, self.appended(row("x"))), f"{shard} (M 100644 -> 100755)"),
            # Valid JSON rows, refused for the path, not the bytes.
            ("a row in a file that is not JSON Lines",
             lambda: self.write({"README": (json.dumps(row("x")) + "\n").encode()}), "README (A 000000 -> 100644)"),
            ("an appended array", self.appended("[1]"), f"{shard} (M 100644 -> 100644)"),
            ("an appended fragment", self.appended('{"run_id": '), f"{shard} (M 100644 -> 100644)"),
            ("a symlinked shard", lambda: os.symlink("series", self.repo / "link.jsonl"),
             "link.jsonl (A 000000 -> 120000)"),
            ("a carrier that is not canonical",
             lambda: self.write({**other, other_carrier: other[other_carrier] + b"\n"}),
             f"{other_carrier} (A 000000 -> 100644)"),
            ("a refused input under another prefix", lambda: self.write({misplaced: refused}),
             f"{misplaced} (A 000000 -> 100644)"),
            ("an executable refused input",
             chmod(series.quarantine_input_path(digest),
                   lambda: self.write({series.quarantine_input_path(digest): refused})),
             f"{series.quarantine_input_path(digest)} (A 000000 -> 100755)"),
            ("an executable object",
             chmod(next(path for path in other if "/objects/" in path), lambda: self.write(other)),
             f"{next(path for path in other if '/objects/' in path)} (A 000000 -> 100755)"),
            ("a published object changed", lambda: self.write({obj: b"other\n"}), f"{obj} (M 100644 -> 100644)"),
            ("a published object removed", remove(obj), f"{obj} (D 100644 -> 000000)"),
            ("a published carrier removed", remove(carrier), f"{carrier} (D 100644 -> 000000)"),
            ("a published refused input changed", lambda: self.write({self.refused: b"other\n"}),
             f"{self.refused} (M 100644 -> 100644)"),
        ]
        for name, edit, change in cases:
            with self.subTest(name):
                base = self.base
                head = self.change(edit)
                self.assertEqual((None, f"{change} is not appended data"), self.appends(head, **self.RUN))
                self.base = base
        ours = DirectLedgerFixture._proof_files("this-run")
        our_carrier = next(path for path in ours if "/runs/" in path)
        for name, names in (("of this run", {"runs": {"this-run"}}),
                            ("for this event", {"events": {"this-run-event"}})):
            with self.subTest(f"another carrier {name}"):
                head = self.change(lambda: self.write(ours))
                self.assertEqual((None, f"{our_carrier} is another finalized carrier of run this-run "
                                        "(event this-run-event)"), self.appends(head, **names))
                self.assertEqual(set(ours), set(self.appends(head)[0]))
        with self.subTest("a path that is not UTF-8"):
            head = self.change(lambda: (self.repo / os.fsdecode(b"\xff.jsonl")).write_text(json.dumps(row("x")) + "\n"))
            self.assertEqual((None, f"the changes from {self.base} to {head} cannot be listed"),
                             self.appends(head, **self.RUN))
        with self.subTest("an appended file over the limit"):
            head = self.change(self.appended(row("x")))
            with mock.patch.object(mirror, "_APPENDED_FILE_LIMIT", 64):
                self.assertEqual((None, f"{shard} (M 100644 -> 100644) is not appended data"),
                                 self.appends(head, **self.RUN))
            self.assertEqual([shard], list(self.appends(head, **self.RUN)[0]))
        with self.subTest("a main that does not descend from the base"):
            head = self.change(self.appended(row("x")))
            self.assertEqual((None, f"{self.base} does not descend from {head}"),
                             mirror._appends_since(self.repo, head, self.base, **self.RUN))


class SkippedRowSummaryTest(unittest.TestCase):
    PREFIX = "  skipped /tmp/s.json:rows["
    SOURCE = ("snapshot /tmp/s.json (sha256 " + "a" * 64 + ", ledger commit " + "b" * 40
              + "), reproducible with scorecard tool " + "c" * 40 + " on measured source " + "d" * 40)
    MISSING = "stress-series/v1 does not record machine_shortname, kernel_version, and host_capabilities"
    UNMATCHED = "no exact test/mode/backend match in the tracked manifest"

    @staticmethod
    def series_row(identifier: str, run_id: str, cell: str = "a/b/verify/ptrace", **series_fields) -> dict:
        return {"event_id": identifier, "run_id": run_id, "producer": "validate",
                "series": {"cell": cell, "run_index": 1, "tree": "d" * 40, **series_fields}}

    @staticmethod
    def record(index: int, value: dict, reason: str) -> str:
        """The line scorecard.rs prints for a skipped snapshot row (SeriesRow::label)."""
        fields = value["series"]
        return (f"  skipped /tmp/s.json:rows[{index}]:{value['event_id']}: {fields['cell']} from "
                f"{value['producer']} run {value['run_id']} repetition {fields['run_index']} @ "
                f"{fields['tree']}: {reason}\n")

    def summarize(self, text: str, rows: list) -> str:
        return mirror.summarize_skipped_rows(text, prefix=self.PREFIX, source=self.SOURCE, rows=rows)

    def test_skipped_snapshot_rows_become_one_summary_with_exact_counts_and_examples(self):
        quoted = 'detcore tree must be a lowercase 40-hex object id, got "x @ ' + "e" * 40 + ': y"'
        rows = [self.series_row("series-1", "r1"), self.series_row("series-2", "r2", "a/c/verify/ptrace"),
                self.series_row("series-3", "r3", "a/d/verify/ptrace"), self.series_row("series-5", "r5"),
                self.series_row("series-6", "r6")]
        lines = [
            "compatibility scorecard: projected 2 cell(s) from 5 evidence row(s)\n",
            self.record(0, rows[0], self.MISSING),
            "  1 row(s) were already represented by one exact recorded run/attempt\n",
            self.record(1, rows[1], self.UNMATCHED),
            self.record(2, rows[2], self.MISSING),
            "    no canonical comparison: kept as is\n",
            "  skipped c-programs/x/verify@ptrace: outcome \"errored\" produced no comparison\n",
            f"  skipped /tmp/other.json:rows[0]:series-4: a/e/verify/ptrace @ {'d' * 40}: {self.MISSING}\n",
            # The reason is everything after the row's label, " @ <hex>: " and all.
            self.record(3, rows[3], quoted),
            # Not the label of the row it names, an index past the rows, an
            # index with a leading zero, no reason: relayed as printed.
            self.record(4, self.series_row("series-6", "another run"), self.MISSING),
            self.record(5, rows[0], self.MISSING).replace("rows[5]", "rows[9]"),
            self.record(0, rows[0], self.MISSING).replace("rows[0]", "rows[00]"),
            self.record(4, rows[4], ""),
            "final line without a newline",
        ]
        self.assertEqual("".join([
            lines[0],
            f"  skipped 4 row(s) of {self.SOURCE}, for 3 reason(s); each reason below has its exact "
            "count and its first row:\n",
            f"    2 row(s): {self.MISSING}\n      first:{lines[1][1:]}",
            f"    1 row(s): {quoted}\n      first:{lines[8][1:]}",
            f"    1 row(s): {self.UNMATCHED}\n      first:{lines[3][1:]}",
            lines[2], *lines[5:8], *lines[9:],
        ]), self.summarize("".join(lines), rows))

    def test_a_record_that_spans_lines_is_relayed_unchanged(self):
        # A row holding a newline anywhere may print its record across lines:
        # in an identifier the renderer interpolates, or in a field a reason
        # quotes. None of its lines is folded, so each count stays exact. Lines
        # end at "\n" only, as println! ends them.
        rows = [self.series_row("series-1", "r1"), self.series_row("series-2", "run\nsecond line"),
                self.series_row("series-3", "r3", failure="first\nsecond"), self.series_row("series-4", "r4"),
                self.series_row("series-5 \x85\rtail", "r5")]
        spanning = self.record(1, rows[1], "reason one")
        quoting = self.record(2, rows[2], "reason one")
        continued = self.record(2, rows[2], "reason one\ncontinued from the row's failure")
        lines = [self.record(0, rows[0], "reason one"), spanning, quoting, continued,
                 self.record(3, rows[3], "reason one"), self.record(4, rows[4], "reason one")]
        self.assertEqual(2, spanning.count("\n"))
        self.assertEqual(
            f"  skipped 3 row(s) of {self.SOURCE}, for 1 reason(s); each reason below has its exact "
            f"count and its first row:\n    3 row(s): reason one\n      first:{lines[0][1:]}"
            + spanning + quoting + continued,
            self.summarize("".join(lines), rows))

    def test_output_without_skipped_snapshot_rows_is_relayed_unchanged(self):
        rows = [self.series_row("series-1", "r1")]
        text = ("compatibility scorecard: projected 1 cell(s)\n"
                "    skipped /tmp/s.json:rows[0]: is not at this indent\n"
                "  skipped c-programs/x/verify@ptrace: outcome \"errored\" produced no comparison\n"
                + self.record(0, rows[0], "reason").replace("/tmp/s.json", "/tmp/other.json"))
        self.assertEqual(text, self.summarize(text, rows))
        self.assertEqual("", self.summarize("", rows))
        line = self.record(0, rows[0], "reason")
        self.assertEqual(line, self.summarize(line, []))
        for name, broken in (("no series", {"event_id": "series-1", "run_id": "r1", "producer": "validate"}),
                             ("a run index that is not a number", self.series_row("series-1", "r1", run_index="1")),
                             ("a boolean run index", self.series_row("series-1", "r1", run_index=True)),
                             ("a missing producer", {k: v for k, v in rows[0].items() if k != "producer"}),
                             ("not an object", ["series-1"])):
            with self.subTest(name):
                self.assertEqual(line, self.summarize(line, [broken]))
        self.assertNotEqual(line, self.summarize(line, rows))


class ScorecardTimeoutTest(unittest.TestCase):
    """The render's bound is the lifecycle remainder less a fixed reserve."""

    def test_without_a_deadline_the_render_gets_a_full_lifecycle_less_the_reserve(self):
        self.assertEqual(2_400, mirror.LIFECYCLE_TIMEOUT_SECONDS)
        self.assertEqual(240, mirror.SCORECARD_RESERVE_SECONDS)
        self.assertEqual(2_160.0, mirror.scorecard_timeout(None))

    def test_with_a_deadline_the_render_gets_the_remainder_less_the_reserve(self):
        deadline = 10_000 * 1_000_000_000
        # remaining seconds -> render seconds: the full 240 s reserve down to
        # 480 s left, then an even split of whatever is left.
        for remaining, expected in ((9_000, 8_760.0), (1_000, 760.0), (480, 240.0),
                                    (400, 200.0), (30, 15.0), (1, 0.5)):
            with self.subTest(remaining=remaining), \
                    mock.patch.object(mirror.time, "monotonic_ns",
                                      return_value=deadline - remaining * 1_000_000_000):
                self.assertEqual(expected, mirror.scorecard_timeout(deadline))

    def test_a_passed_deadline_refuses_before_the_render(self):
        deadline = 10_000 * 1_000_000_000
        for now_seconds in (10_000, 11_000):
            with self.subTest(now=now_seconds), \
                    mock.patch.object(mirror.time, "monotonic_ns", return_value=now_seconds * 1_000_000_000), \
                    self.assertRaisesRegex(mirror.MirrorRefused, "deadline passed before the scorecard render"):
                mirror.scorecard_timeout(deadline)

    def test_the_render_bound_fits_inside_the_lifecycle(self):
        self.assertLess(mirror.SCORECARD_RESERVE_SECONDS, mirror.LIFECYCLE_TIMEOUT_SECONDS)
        self.assertLessEqual(mirror.DEFAULT_TIMEOUT_SECONDS, mirror.LIFECYCLE_TIMEOUT_SECONDS)

    def test_validate_finalization_uses_the_same_lifecycle_ceiling(self):
        # mirror caps any caller deadline at its own ceiling, so a smaller
        # value in the validate finalizer would cut the render short there.
        source = (mirror.TOOL_ROOT / "ci-hub/validate/start_unit.py").read_text()
        values = [ast.literal_eval(node.value) for node in ast.parse(source).body
                  if isinstance(node, ast.Assign)
                  and any(getattr(target, "id", None) == "LEDGER_MIRROR_LIFECYCLE_SECONDS"
                          for target in node.targets)]
        self.assertEqual([mirror.LIFECYCLE_TIMEOUT_SECONDS], values)


class PublicationOutcomeRecordTest(unittest.TestCase):
    """A finalized publication leaves its report for the run's later callers, and nothing else does.

    start_unit.finalize_ledger_mirror adopts the recorded report instead of
    running this publisher (and its rust-script --force census rebuild) again
    for the same run; see ci-hub/lib/ledger_publication_outcome.py.
    """

    RUN = "validate-ops-tick-4b1894e81cfb-18e5730802ad"
    TARGET = "4" * 40
    TRANSACTION = "5" * 40
    TOOL = "c" * 40
    SERIES = "e" * 40
    FILES = {"scorecard/SCORECARD.md": "1" * 64, "scorecard/cells.json": "2" * 64,
             "scorecard/parity.json": "3" * 64}

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.parent = Path(temporary.name).resolve()
        # The receipt mirror itself writes after a finalized render's push.
        receipt = mirror.completion_receipt(
            run_id=self.RUN, target=self.TARGET, tool_commit=self.TOOL, transaction=self.TRANSACTION,
            series_commit=self.SERIES, latest=b"{}\n", finalized_row=b"{}\n", files=self.FILES)
        path = mirror._receipt_path(self.parent, self.RUN)
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps(receipt, indent=1, sort_keys=True) + "\n")

    def report(self) -> dict:
        return {"state": "published", "commit": self.TRANSACTION, "series_commit": self.SERIES,
                "scorecard": {"state": "published", "files": dict(self.FILES),
                              "hermit_commit": self.TARGET, "tool_hermit_commit": self.TOOL}}

    def outcome(self) -> Path:
        return mirror.ledger_publication_outcome.outcome_path(self.parent, self.RUN)

    def main(self, *extra: str) -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = mirror.main(["--parent", str(self.parent), "--source-checkout", str(self.parent / "source"),
                                "--target", self.TARGET, "--results", str(self.parent / "results"),
                                "--bounded-worker", *extra])
        return code, stdout.getvalue(), stderr.getvalue()

    def test_the_receipt_the_outcome_checks_is_the_one_mirror_writes(self):
        self.assertEqual(mirror._receipt_path(self.parent, self.RUN),
                         mirror.ledger_publication_outcome.receipt_path(self.parent, self.RUN))
        self.assertEqual((series.spool_dir(self.parent), series.parity_spool_dir(self.parent),
                          series.quarantine_dir(self.parent), series.parity_deferred_dir(self.parent)),
                         mirror.ledger_publication_outcome.spool_dirs(self.parent))

    def test_a_finalized_publication_is_recorded_and_adoptable(self):
        report = self.report()
        with mock.patch.object(mirror, "_verify_lifecycle_worker"), \
                mock.patch.object(mirror, "submodule_remote", return_value="fixture"), \
                mock.patch.object(mirror, "mirror", return_value=report):
            code, stdout, stderr = self.main("--finalized-run-id", self.RUN)
        self.assertEqual(0, code, stderr)
        self.assertEqual(report, json.loads(stdout))
        self.assertIn(f"publication outcome {self.outcome()} recorded for run {self.RUN}", stderr)
        adopted, why = mirror.ledger_publication_outcome.adopt(self.parent, self.RUN, self.TARGET)
        self.assertIsNotNone(adopted, why)
        self.assertEqual(report, adopted.report)
        self.assertEqual(self.TRANSACTION, adopted.transaction)

    def test_a_derived_failure_is_not_recorded(self):
        report = self.report()
        report.update(state="derived-failed", scorecard={"state": "failed", "error": "list index out of range"})
        with mock.patch.object(mirror, "_verify_lifecycle_worker"), \
                mock.patch.object(mirror, "submodule_remote", return_value="fixture"), \
                mock.patch.object(mirror, "mirror", return_value=report):
            code, _stdout, stderr = self.main("--finalized-run-id", self.RUN)
        self.assertEqual(1, code)
        self.assertIn("no publication outcome recorded; a later caller publishes again", stderr)
        self.assertFalse(self.outcome().exists())

    def spool_series_batch(self) -> Path:
        """A series spool batch of the run, under the name series._batch_name gives it."""
        path = series.spool_dir(self.parent) / series._batch_name([{"run_id": self.RUN}])
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{}\n")
        return path

    def spool_parity_refusal(self) -> Path:
        """A parity refusal of the run, spooled as series.py append-parity spools it."""
        parity = series.parity_ledger_module()
        event = parity.build_refusal(
            producer="validate", run_id=self.RUN, hermit_sha=self.TARGET, emitted_at="2026-10-04T01:52:38Z",
            source_tree_dirty=False, host="devbig014", input_bytes=b"not parity\n", input_lines=1,
            error="fixture refusal")
        spool = series.parity_spool_dir(self.parent)
        return spool / parity.append_refusal(spool, event)["batch"]

    def spool_refused_input(self) -> Path:
        """A refused series input of the run, written by series.append_quarantine."""
        record = series.quarantine_record(
            stage="projection", run_id=self.RUN, producer="validate", tree="a" * 40,
            emitted_at="2026-10-04T01:52:38Z", error="MalformedRow", reason="fixture refusal",
            inputs=[series.quarantine_input(1, b"not json")], host="devbig014")
        return Path(series.append_quarantine(self.parent, self.RUN, [record])["path"])

    def test_a_publication_with_a_spooled_input_of_the_run_is_not_recorded(self):
        for name, spool in (("series batch", self.spool_series_batch),
                            ("parity refusal", self.spool_parity_refusal),
                            ("refused input", self.spool_refused_input)):
            with self.subTest(name):
                spooled = spool()
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    recorded = mirror.record_publication_outcome(self.parent, self.RUN, self.TARGET, self.report())
                self.assertIsNone(recorded)
                self.assertIn("no publication outcome recorded; a later caller publishes again: "
                              "inputs of the run are still spooled: ", stderr.getvalue())
                self.assertIn(str(spooled), stderr.getvalue())
                self.assertFalse(self.outcome().exists())
                spooled.unlink()
        # With nothing of the run left, the same publication is recorded.
        self.assertEqual(self.outcome(), mirror.record_publication_outcome(
            self.parent, self.RUN, self.TARGET, self.report()))

    def test_only_a_finalized_ordinary_publication_reaches_the_recorder(self):
        cases = {
            "finalized": (["--finalized-run-id", self.RUN], True),
            "portable proof only": (["--finalized-run-id", self.RUN, "--portable-proof-only"], False),
            "not finalized": ([], False),
        }
        for name, (extra, expected) in cases.items():
            with self.subTest(name), mock.patch.object(mirror, "_verify_lifecycle_worker"), \
                    mock.patch.object(mirror, "submodule_remote", return_value="fixture"), \
                    mock.patch.object(mirror, "mirror", return_value=self.report()), \
                    mock.patch.object(mirror, "record_publication_outcome") as recorder:
                code, _stdout, stderr = self.main(*extra)
                self.assertEqual(0, code, stderr)
                if expected:
                    recorder.assert_called_once_with(self.parent, self.RUN, self.TARGET, self.report())
                else:
                    recorder.assert_not_called()

    def test_a_refused_publication_reaches_no_recorder(self):
        with mock.patch.object(mirror, "_verify_lifecycle_worker"), \
                mock.patch.object(mirror, "submodule_remote", return_value="fixture"), \
                mock.patch.object(mirror, "mirror", side_effect=mirror.MirrorRefused("refused")), \
                mock.patch.object(mirror, "failure_report", return_value=None), \
                mock.patch.object(mirror, "record_publication_outcome") as recorder:
            code, _stdout, stderr = self.main("--finalized-run-id", self.RUN)
        self.assertEqual(1, code)
        self.assertIn("ledger publication: FAILED: refused", stderr)
        recorder.assert_not_called()
        self.assertFalse(self.outcome().exists())


def real_finalized_suite(provider_path: Path) -> unittest.TestSuite:
    """Run the expensive native transaction contract separately from doubles.

    The provider records prewarmed executable and source digests. Compilation
    happens outside this test and outside the 60-second publication lock. The
    measured repository below is small contract data, not a product checkout or
    an admitted validation. All temporary repositories remain under evidence_root.
    """
    import ledger
    import validate_rows

    provider = json.loads(provider_path.read_text())

    class RealFinalizedTransactions(unittest.TestCase):
        def checked_provider(self):
            self.assertEqual(provider["schema"], "finalized-concurrency-provider/v1")
            for name, digest in provider["source_files"].items():
                self.assertEqual(hashlib.sha256(Path(name).read_bytes()).hexdigest(), digest, name)
            for name, value in provider["executables"].items():
                path = Path(value["path"])
                self.assertTrue(path.is_absolute() and path.is_file(), name)
                self.assertTrue(os.access(path, os.X_OK), name)
                self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), value["sha256"], name)
            self.assertEqual(git(self.tool, "rev-parse", "HEAD"), provider["hermit_commit"])
            self.assertEqual(mirror._committed_tool_source(self.tool), provider["hermit_commit"])

        def command(self, argv, *, cwd=None, timeout=45):
            result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True,
                                    timeout=timeout, check=False)
            self.calls.append({"argv": list(map(str, argv)), "cwd": str(cwd),
                               "returncode": result.returncode,
                               "stdout": result.stdout, "stderr": result.stderr})
            self.assertEqual(result.returncode, 0, self.calls[-1])
            return result.stdout

        def setUp(self):
            self.tool = Path(provider["hermit_root"]).resolve()
            self.checked_provider()
            self.calls = []
            self.trace = []
            self.root = Path(tempfile.mkdtemp(
                prefix="finalized-transactions-", dir=provider["evidence_root"]))
            self.addCleanup(self.retain_evidence)
            self.parent = self.root / "state"
            self.source = self.parent / "hermit"
            self.remote = self.root / "remote.git"
            self.ledger = self.parent / "hermit_test_ledger"
            config = self.root / "gitconfig"
            config.write_text(
                '[user]\n name = Fixture\n email = fixture@example.invalid\n'
                '[init]\n defaultBranch = main\n'
                f'[url "{self.remote}"]\n insteadOf = {mirror.REMOTE_URL}\n')
            environment = {
                "GIT_CONFIG_GLOBAL": str(config), "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_ALLOW_PROTOCOL": "file",
                "WITH_PROXY": "/usr/bin/env", "PYTHONDONTWRITEBYTECODE": "1",
                "RUSTUP_TOOLCHAIN": provider["toolchain"], "RUSTUP_AUTO_INSTALL": "0",
                "CI_HUB_LEDGER_EVENT_CHECKER": provider["executables"]["ledger"]["path"],
            }
            self.enterContext(mock.patch.dict(os.environ, environment))
            for key in tuple(os.environ):
                if key.startswith("GIT_") and key not in environment:
                    os.environ.pop(key)
            for key in (series.LEDGER_ROOT_ENV, "DEV_HERMIT_TOOL_AUTHORITY",
                        mirror.LOCK_FD_ENV):
                os.environ.pop(key, None)
            temporary = self.root / "temporary"
            temporary.mkdir()
            os.environ["TMPDIR"] = str(temporary)
            self.enterContext(mock.patch.object(tempfile, "tempdir", str(temporary)))
            self.command(["git", "init", "--bare", "-q", str(self.remote)])
            self.command(["git", "init", "-q", str(self.parent)])
            (self.parent / ".gitmodules").write_text(
                '[submodule "hermit_test_ledger"]\n path = hermit_test_ledger\n'
                f' url = {mirror.REMOTE_URL}\n')
            (self.parent / ".gitignore").write_text("ignored/\nhermit/\nhermit_test_ledger/\n")
            self.command(["git", "init", "-q", str(self.source)])
            self.install_transport()
            self.make_source_fixture()
            self.make_ledger_fixture()
            self.assertEqual(series.shared_parent_root(self.parent), self.parent)
            self.assertEqual(series.ledger_root(self.parent), self.ledger)
            self.assertEqual(series.spool_dir(self.parent),
                             self.parent / "ignored/ci-hub/series-spool")
            self.inputs = {name: self.make_run(name, second) for name, second in
                           (("finalized-a", "00"), ("finalized-b", "01"))}
            git(self.parent, "add", ".gitmodules", ".gitignore", "ledger")
            git(self.parent, "commit", "-qm", "two failed partial finalized fixture records")
            self.parent_head = git(self.parent, "rev-parse", "HEAD")
            self.canonical_before = {
                name: mirror.finalized_producer_row(self.parent, name, self.target, self.source)
                for name in self.inputs
            }

        def write_json(self, path, value):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n")

        def install_transport(self):
            """Dispatch only exact compiler argv to real, verified native bodies."""
            directory = self.root / "bin"
            directory.mkdir()
            script_map = {
                str(self.tool / "ci/compat-envelope/scorecard.rs"):
                    provider["executables"]["scorecard"]["path"],
                str(MODULE.parent / "check-series-rows.rs"):
                    provider["executables"]["series"]["path"],
            }
            compiler = directory / "rust-script"
            compiler.write_text(
                "#!/usr/bin/python3\nimport os, sys\n"
                f"mapping = {script_map!r}\n"
                f"assert sys.argv[1:4] == ['--force', '--toolchain', {provider['toolchain']!r}]\n"
                "assert os.environ['RUSTUP_AUTO_INSTALL'] == '0'\n"
                "if sys.argv[5:] == ['--help']:\n"
                " assert 'DEV_HERMIT_TEST_LEDGER_ROOT' not in os.environ\n"
                " assert 'DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD' not in os.environ\n"
                "binary = mapping[sys.argv[4]]\n"
                "os.execv('/usr/bin/timeout', ['timeout', '--kill-after=5s', '45s', binary, *sys.argv[5:]])\n")
            compiler.chmod(0o755)
            cargo = directory / "cargo"
            expected = ["run", "--quiet", "-p", "hermit-manifest-plan", "--manifest-path",
                        str(self.tool / "Cargo.toml"), "--target-dir", str(self.parent / "target"),
                        "--", "--root", str(self.source), "--format", "matrix-json"]
            cargo.write_text(
                "#!/usr/bin/python3\nimport os, sys\n"
                f"assert sys.argv[1:] in [{expected!r}, {expected[:9] + ['--help']!r}], sys.argv\n"
                "if sys.argv[10:] == ['--help']:\n"
                " assert 'DEV_HERMIT_TEST_LEDGER_ROOT' not in os.environ\n"
                " assert 'DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD' not in os.environ\n"
                f"binary = {provider['executables']['manifest']['path']!r}\n"
                "os.execv('/usr/bin/timeout', ['timeout', '--kill-after=5s', '45s', binary, *sys.argv[10:]])\n")
            cargo.chmod(0o755)
            checker = directory / "ledger-checker"
            checker.write_text(
                "#!/usr/bin/python3\nimport os\n"
                f"binary = {provider['executables']['ledger']['path']!r}\n"
                "os.execv('/usr/bin/timeout', ['timeout', '--kill-after=5s', '45s', binary])\n")
            checker.chmod(0o755)
            os.environ["CI_HUB_LEDGER_EVENT_CHECKER"] = str(checker)
            os.environ["PATH"] = str(directory) + os.pathsep + os.environ["PATH"]
            os.environ["CARGO_TARGET_DIR"] = str(self.parent / "target")

        def make_source_fixture(self):
            """One declared cell; native plan/catalogue generation stays real."""
            self.cell = {"lane": "portable", "category": "fixture",
                         "test": "fixture/no-result", "mode": "verify", "backend": "ptrace"}
            backends = ["ptrace", "dbt", "kvm", "sabre", "liteinst"]
            modes = {}
            for mode in ("verify", "replay", "chaos", "naked", "custom"):
                enabled = ["ptrace"] if mode == "verify" else []
                all_backends = ["native"] if mode == "naked" else backends
                modes[mode] = {
                    "ci": mode == "verify", "backends_enabled": enabled,
                    "backends_disabled": {b: "Synthetic transaction fixture does not execute guests"
                                          for b in all_backends if b not in enabled},
                }
                if mode != "verify":
                    modes[mode]["ci_disabled_reason"] = "Synthetic transaction fixture has one declared cell"
            manifests = self.source / "tests/e2e/manifests"
            self.write_json(manifests / "defaults.yaml", {
                "schema": 3, "timeout_seconds": 15, "cpu_timeout_seconds": 10})
            self.write_json(manifests / "fixture.yaml", {
                "schema": 3, "bucket": "fixture", "test": [{
                    "id": self.cell["test"], "description": "No guest is executed by this contract fixture",
                    "lane": "portable", "requires": [], "occasional": False,
                    "program": "tests/fixture.sh", "observation": {
                        "status": True, "stdout": True, "stderr": True, "artifacts": []},
                    "modes": modes,
                }]})
            (self.source / "tests/fixture.sh").write_text("#!/bin/sh\nexit 75\n")
            inventory = "tests/e2e/manifests/inventory/test-files.json"
            paths = ["tests/fixture.sh", "tests/e2e/manifests/defaults.yaml",
                     "tests/e2e/manifests/fixture.yaml", inventory]
            self.write_json(self.source / inventory, {"schema": 2, "files": [
                {"path": p, "disposition": "manifest-test" if p == "tests/fixture.sh" else "support",
                 "runner": "transaction-fixture", "why": "Synthetic data, no guest execution"}
                for p in sorted(paths)]})
            self.write_json(self.source / "ci/ci-reason-baseline.json", {
                "schema": 1, "unreasoned_ci_false_cells": []})
            self.write_json(self.source / "ci/matrix-symmetry-baseline.json", {
                "schema": 1, "asymmetric_manifest_tests": [], "backend_private_guest_files": []})
            self.write_json(self.source / "ci/expected-e2e-plan.json", {"cells": [self.cell]})
            (self.source / "detcore").mkdir()
            (self.source / "detcore/fixture-input").write_text("Transaction contract data only\n")
            (self.source / "ci/compat-envelope").mkdir()
            git(self.source, "add", "tests", "ci", "detcore")
            git(self.source, "commit", "-qm", "synthetic measured input for ledger transaction")
            self.command([provider["executables"]["scorecard"]["path"], "update"], cwd=self.source)
            git(self.source, "add", "SCORECARD.md", "ci/compat-envelope/cells.json")
            git(self.source, "commit", "-qm", "native generated fixture catalogue")
            self.target = git(self.source, "rev-parse", "HEAD")
            git(self.source, "update-ref", "refs/remotes/origin/main", self.target)
            self.assertEqual(git(self.source, "status", "--porcelain"), "")

        def make_ledger_fixture(self):
            seed = self.root / "seed"
            self.command(["git", "init", "-q", str(seed)])
            catalogue = json.loads((self.source / "ci/compat-envelope/cells.json").read_text())
            self.assertEqual(catalogue["schema"], 9)
            cells = [{**cell, "observations": []} for cell in catalogue["cells"]]
            self.write_json(seed / "scorecard/cells.json", {"schema": 8, "cells": cells})
            (seed / "scorecard/SCORECARD.md").write_text("Empty transaction fixture history\n")
            (seed / "series/hermit/fixture").mkdir(parents=True)
            (seed / "series/hermit/fixture/2026-09.jsonl").write_text("")
            (seed / "retained-before.txt").write_text("Existing unrelated ledger data\n")
            git(seed, "add", "scorecard", "series", "retained-before.txt")
            git(seed, "commit", "-qm", "seed existing ledger data")
            git(seed, "remote", "add", "origin", str(self.remote))
            git(seed, "push", "-q", "origin", "main")
            self.seed = git(seed, "rev-parse", "HEAD")
            self.command(["git", "clone", "-q", mirror.REMOTE_URL, str(self.ledger)])
            self.assertEqual(git(self.ledger, "remote", "get-url", "--push", "origin"), str(self.remote))

        def make_run(self, name, second):
            report = {
                "verified": False, "bitwise_parity": False, "verdict": "no_result",
                "no_result_reason": {"kind": "not_run"}, "infrastructure_error": None,
                "comparison": None, "compared_log_messages": None, "runtime": None,
                "guest_exit_code": None, "guest_signal": None,
                "first_divergent_scheduler_turn": None, "first_divergent_virtual_nanoseconds": None,
                "first_divergent_record": None, "first_divergent_syscall": None,
                "first_divergent_left_message": None, "first_divergent_right_message": None,
            }
            report_bytes = json.dumps(report, sort_keys=True, separators=(",", ":"))
            invocation = {"argv": ["hermit", "run", "--verify", "fixture"],
                          "guest_argv": ["fixture"], "env": {"LC_ALL": "C"}, "cwd": "/repo",
                          "shell_command": "cd /repo && env LC_ALL=C hermit run --verify fixture"}
            raw = {**self.cell, **invocation, "schema": 4, "run_id": name, "attempt": 1,
                   "hermit_sha": self.target, "source_tree_dirty": False,
                   "binary_sha256": hashlib.sha256(b"synthetic unexecuted binary fixture").hexdigest(),
                   "test_sha256": hashlib.sha256(b"fixture").hexdigest(),
                   "machine_shortname": "fixture", "kernel_version": "fixture-no-execution",
                   "host_capabilities": {
                       capability: {"present": False, "evidence": "Synthetic fixture — no hardware probe"}
                       for capability in ("cpuid-faulting", "kvm")},
                   "classification": "required", "outcome": "ERROR",
                   "result": None, "failure_class": "no_result", "timeout_seconds": 15,
                   "execution_cpu_timeout_seconds": 10, "execution_wall_timeout_seconds": 15,
                   "log_level": "info", "effective_args": invocation["argv"][1:], "relaxations": [],
                   "attempts": [{**invocation, "index": "1", "outcome": "ERROR",
                                 "error_kind": "incomplete-verification-evidence", "status": 75,
                                 "signal": None, "timed_out": False,
                                 "verification_report": report_bytes,
                                 "verification_report_sha256": hashlib.sha256(report_bytes.encode()).hexdigest()}]}
            results = self.parent / "ignored/results" / name
            self.write_json(results / "results.jsonl", raw)
            data = (results / "results.jsonl").read_bytes()
            stamp = f"2026-09-21T12:00:{second}Z"
            row = {"schema_version": 5, "repo": "hermit", "producer": "hermit-validate-rs",
                   "host": "fixture", "run_id": name, "record_id": name,
                   "commit": self.target, "tree_dirty": False, "commit_anchored": True,
                   "cwd": "{{WORKSPACE_ROOT}}/hermit", "started_at": stamp, "finished_at": stamp,
                   "result": "fail", "raw_result": "fail", "executed_tests": None,
                   "passed_tests": None, "gates_expected": 3, "gates_run": 1,
                   "raw_result_input_census_v1": {
                       "schema": 1, "run_id": name, "hermit_sha": self.target, "files": [{
                           "path": "results.jsonl", "bytes": len(data),
                           "sha256": hashlib.sha256(data).hexdigest(),
                           "rows": [{"line": 1, "cell": self.cell, "attempt": 1}]}]}}
            event = validate_rows._event_for_row(row, self.parent)
            path = self.parent / "ledger/hermit/fixture/2026-09.jsonl"
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("a") as stream:
                stream.write(json.dumps(event, sort_keys=True) + "\n")
            projected = series.rows_from_cell_results(
                [raw], parent=self.parent, producer="validate", run_id=name, tree=self.target,
                emitted_at=stamp, hermit_checkout=self.source)
            self.assertEqual(len(projected), 1)
            return {"results": results, "raw": raw, "series": projected, "row": row}

        def append(self, name):
            path = series.spool_dir(self.parent) / f"{name}.jsonl"
            series.append_cell_rows(self.parent, path, self.inputs[name]["series"])

        def publish(self, name):
            return mirror.mirror(self.parent, self.source, self.target, mirror.REMOTE_URL,
                                 net_wrapper="/usr/bin/env", tool_checkout=self.tool,
                                 results=self.inputs[name]["results"], finalized_run_id=name)

        def retain_evidence(self):
            self.write_json(self.root / "execution.json", {
                "provider": str(provider_path), "target": getattr(self, "target", None),
                "calls": self.calls, "lock_trace": self.trace,
                "reports": getattr(self, "reports", None),
                "scope": "synthetic schema5 failed/partial records; no admitted run or guest execution",
            })
            print(f"real finalized transaction evidence: {self.root}", file=sys.stderr)

        def test_contended_finalized_transactions_preserve_raw_rows_and_derived_history(self):
            a_at_writer = threading.Event()
            b_contended = threading.Event()
            preparations_ready = threading.Barrier(3)
            release_preparations = threading.Event()
            local = threading.local()
            real_flock, real_network, real_run = mirror.fcntl.flock, mirror._network, mirror._run
            lock_path = self.ledger / ".git" / mirror.PUBLICATION_LOCK

            def observed_flock(fd, operation):
                watched = (operation == mirror.fcntl.LOCK_EX | mirror.fcntl.LOCK_NB
                           and getattr(local, "name", None) is not None and lock_path.exists()
                           and os.fstat(fd).st_ino == lock_path.stat().st_ino
                           and os.fstat(fd).st_dev == lock_path.stat().st_dev)
                try:
                    value = real_flock(fd, operation)
                except BlockingIOError:
                    if watched:
                        self.trace.append({"run": local.name, "event": "contended", "fd": fd,
                                           "inode": os.fstat(fd).st_ino, "time": time.monotonic_ns()})
                        if local.name == "finalized-b":
                            b_contended.set()
                    raise
                if watched:
                    self.trace.append({"run": local.name, "event": "acquired", "fd": fd,
                                       "inode": os.fstat(fd).st_ino, "time": time.monotonic_ns()})
                return value

            def observed_network(command, **kwargs):
                if (command[0] == "rust-script" and command[-1] == "--help"
                        and not getattr(local, "prepared", False)):
                    local.prepared = True
                    self.assertNotIn(mirror.LOCK_FD_ENV, kwargs["environment"])
                    self.assertNotIn(series.LEDGER_ROOT_ENV, kwargs["environment"])
                    self.assertEqual(kwargs.get("pass_fds", ()), ())
                    preparations_ready.wait(15)
                    self.assertTrue(release_preparations.wait(15))
                    result = real_network(command, **kwargs)
                    # Keep the existing A-writer/B-contention opponent after
                    # both real native preparations have run outside the lock.
                    if local.name == "finalized-b":
                        self.assertTrue(a_at_writer.wait(15), "A did not reach the real writer")
                    return result
                if command[0] == "git" and any(part in ("fetch", "push") for part in command):
                    self.assertEqual(git(kwargs["cwd"], "remote", "get-url", "--push", "origin"), str(self.remote))
                    self.assertEqual(os.environ["GIT_ALLOW_PROTOCOL"], "file")
                self.assertNotIn("project-and-observe-results", command, "the renderer never gets the wrapper")
                return real_network(command, **kwargs)

            def observed_run(command, **kwargs):
                # The renderer runs directly (not through _network), in its ledger copy.
                if command[0] == "rust-script" and "project-and-observe-results" in command:
                    selected = json.loads(Path(command[command.index("--finalized-row") + 1]).read_text())
                    self.assertEqual(selected["run_id"], local.name)
                    for option, suffix in (("--snapshot", "snapshot"), ("--finalized-row", "row")):
                        content = Path(command[command.index(option) + 1]).read_bytes()
                        (self.root / f"{local.name}-{suffix}.json").write_bytes(content)
                    if local.name == "finalized-a":
                        a_at_writer.set()
                        self.assertTrue(b_contended.wait(15), "B never contended on A's actual lock")
                return real_run(command, **kwargs)

            def publish(name):
                local.name = name
                self.append(name)
                return self.publish(name)

            with mock.patch.object(mirror.fcntl, "flock", side_effect=observed_flock), \
                    mock.patch.object(mirror, "_network", side_effect=observed_network), \
                    mock.patch.object(mirror, "_run", side_effect=observed_run), \
                    ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(publish, name) for name in self.inputs]
                try:
                    preparations_ready.wait(15)
                    with mirror.publication_writer(self.parent, net_wrapper="/usr/bin/env"):
                        self.assertTrue(all(not future.done() for future in futures))
                        self.trace.append({"run": "preparation-observer",
                                           "event": "lock_available_during_both_preparations",
                                           "time": time.monotonic_ns()})
                finally:
                    release_preparations.set()
                self.reports = [future.result(timeout=90) for future in futures]
            for report in self.reports:
                self.assertEqual(report["state"], "published", report)
                self.assertEqual(report["scorecard"]["state"], "published", report)
                git(self.remote, "merge-base", "--is-ancestor", report["commit"], "main")
            acquired = {entry["run"]: entry for entry in self.trace if entry["event"] == "acquired"}
            self.assertEqual(set(acquired), set(self.inputs))
            blocked = next(entry for entry in self.trace if entry["run"] == "finalized-b"
                           and entry["event"] == "contended")
            self.assertNotEqual(acquired["finalized-a"]["fd"], blocked["fd"])
            self.assertEqual(acquired["finalized-a"]["inode"], blocked["inode"])
            self.assertLess(acquired["finalized-a"]["time"], blocked["time"])
            self.assertLess(blocked["time"], acquired["finalized-b"]["time"])
            raw_rows = [json.loads(line) for line in git(
                self.remote, "show", "main:series/hermit/fixture/2026-09.jsonl").splitlines()]
            expected = [value["series"][0] for value in self.inputs.values()]
            self.assertEqual(sorted(raw_rows, key=lambda item: item["event_id"]),
                             sorted(expected, key=lambda item: item["event_id"]))
            cells = json.loads(stored_history(self.remote))
            cell = next(cell for cell in cells["cells"] if all(cell[k] == v for k, v in self.cell.items()))
            observations = cell["observations"]
            invocations = [invocation for observation in observations for invocation in observation["invocations"]]
            self.assertEqual({item["run_id"] for item in invocations}, set(self.inputs))
            self.assertEqual(len(invocations), 2)
            self.assertTrue(all(item.get("result") is None for item in invocations))
            self.assertTrue(all(not item.get("canonical_comparisons") and not item.get("backend_parity_comparisons")
                                for item in observations), "no-result fixture must not earn comparison credit")
            self.assertEqual(cells["projection"]["rows_read"], 2)
            metadata = json.loads(git(self.remote, "show", "main:latest.json"))
            self.assertEqual(metadata["rows"], 2)
            self.assertEqual(metadata["source_commit"], cells["projection"]["source_commit"])
            for path, digest in metadata["scorecard"].items():
                # latest.json names the history by its logical path; main stores it in parts.
                content = (stored_history(self.remote) if path == mirror.scorecard_cells.LOGICAL_PATH
                           else subprocess.check_output(["git", "-C", str(self.remote), "show", f"main:{path}"]))
                self.assertEqual(hashlib.sha256(content).hexdigest(), digest, path)
            self.assertEqual(git(self.remote, "show", "main:retained-before.txt"), "Existing unrelated ledger data")
            git(self.remote, "merge-base", "--is-ancestor", self.seed, "main")
            self.assertEqual(git(self.parent, "rev-parse", "HEAD"), self.parent_head)
            self.assertEqual(git(self.source, "status", "--porcelain"), "")
            for name in self.inputs:
                canonical = mirror.finalized_producer_row(self.parent, name, self.target, self.source)
                self.assertEqual(canonical, self.canonical_before[name])
                row = json.loads(canonical)
                self.assertEqual((row["result"], row["raw_result"], row["executed_tests"],
                                  row["gates_run"], row["gates_expected"]), ("fail", "fail", None, 1, 3))
            self.assertFalse((self.parent / "target").exists(), "native transport must reuse the existing target")
            self.assertEqual(len([line for line in git(self.ledger, "worktree", "list", "--porcelain").splitlines()
                                  if line.startswith("worktree ")]), 2)
            # An actual changed raw input must fail against the retained census,
            # leaving the two completed publications and canonical records intact.
            head = git(self.remote, "rev-parse", "main")
            raw_path = self.inputs["finalized-a"]["results"] / "results.jsonl"
            raw_path.write_bytes(raw_path.read_bytes() + b"\n")
            refused = self.publish("finalized-a")
            self.assertEqual(refused["state"], "derived-failed", refused)
            self.assertIn("census", refused["scorecard"]["error"])
            self.assertEqual(git(self.remote, "rev-parse", "main"), head)
            self.reports.append(refused)
            self.checked_provider()

    return unittest.defaultTestLoader.loadTestsFromTestCase(RealFinalizedTransactions)


class ScorecardUnwrappedGit(unittest.TestCase):
    """The scorecard renderer runs git without the host's telemetry wrapper,
    by the substitution validate steps get (ci-hub/validate/unwrapped_git.py).
    Every binary here is a temporary fake shell script; the host's
    /usr/local/bin is never executed."""

    VERSION = "git version 2.53.0-Meta"

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        local = self.root / "local"
        local.mkdir()
        script = (f'#!/bin/sh\nif [ "$1" = --version ]; then echo {shlex.quote(self.VERSION)}; '
                  'exit 0; fi\necho fake-git "$@"\n')
        self.wrapper = local / "git"
        self.real = local / "git.meta.real"
        for path in (self.wrapper, self.real):
            path.write_text(script, encoding="utf-8")
            path.chmod(0o755)
        self.config = self.root / "scm-telemetry.toml"
        self.config.write_text(f"[binaries]\ngit = {json.dumps(str(self.real))}\n", encoding="utf-8")
        self.state = self.root / "state"
        self.state.mkdir()

    def scorecard_git(self, environment: dict[str, str]):
        return mirror._scorecard_git(self.state, environment, real_git=self.real,
                                     wrapper_config=self.config)

    def test_the_renderer_gets_the_unwrapped_git_and_the_directory_is_removed_after(self) -> None:
        environment = {"PATH": f"{self.wrapper.parent}{os.pathsep}/usr/bin"}
        with self.scorecard_git(environment) as substitution:
            self.assertTrue(substitution.applied, substitution.summary())
            first = Path(environment["PATH"].split(os.pathsep)[0])
            self.assertTrue(first.is_relative_to(self.state / mirror.UNWRAPPED_GIT_RELATIVE))
            self.assertEqual((first / "git").resolve(), self.real.resolve())
            self.assertEqual(environment[mirror.unwrapped_git.NETWORK_GIT_ENV], str(self.wrapper))
        self.assertFalse(first.exists(), "the run-private git directory is removed")

    def test_a_path_whose_git_is_not_the_wrapper_is_left_alone(self) -> None:
        plain = self.root / "plain"
        plain.mkdir()
        (plain / "git").write_text("#!/bin/sh\necho git version 2.52.0\n", encoding="utf-8")
        (plain / "git").chmod(0o755)
        environment = {"PATH": f"{plain}{os.pathsep}/usr/bin"}
        with self.scorecard_git(environment) as substitution:
            self.assertFalse(substitution.applied)
        self.assertEqual(environment, {"PATH": f"{plain}{os.pathsep}/usr/bin"})
        self.assertFalse((self.state / mirror.UNWRAPPED_GIT_RELATIVE).exists())

    def test_a_library_caller_without_a_state_root_is_unchanged(self) -> None:
        environment = {"PATH": str(self.wrapper.parent)}
        with mirror._scorecard_git(None, environment) as substitution:
            self.assertIsNone(substitution)
        self.assertEqual(environment, {"PATH": str(self.wrapper.parent)})


class EmptiedRetentionRemovalTest(unittest.TestCase):
    """prune_displaced removes the files it emptied, and their retention directory, once nothing is kept."""

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.writer = self.root / "writer"
        self.writer.mkdir()

    def retention(self, label: str, files: dict[str, bytes], recorded: list[str]) -> Path:
        retained = self.root / f"writer.displaced-{label}"
        for name, data in files.items():
            (retained / "files" / name).parent.mkdir(parents=True, exist_ok=True)
            (retained / "files" / name).write_bytes(data)
        (retained / "displaced.json").write_text(json.dumps({"displaced_at": 0, "files": [
            {"path": name, "sha256": hashlib.sha256(files[name]).hexdigest(), "size": len(files[name])}
            for name in recorded]}))
        return retained

    def prune(self) -> str:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            mirror.prune_displaced(self.writer, now=mirror.DISPLACED_RETENTION_SECONDS + 1)
        return stderr.getvalue()

    def test_an_emptied_retention_directory_is_removed(self):
        retained = self.retention("plain", {"scorecard/SCORECARD.md": b"stored\n", "latest.json": b"{}\n"},
                                  ["scorecard/SCORECARD.md", "latest.json"])
        self.assertIn("emptied 2 unchanged file(s)", self.prune())
        self.assertFalse(os.path.lexists(retained))

    def test_an_unrecorded_file_keeps_its_directory(self):
        # Bytes HEAD does not store are never recorded, never emptied and
        # never removed; the emptied recorded file still goes.
        retained = self.retention("unstored", {"latest.json": b"{}\n", "draft.json": b"operator\n"},
                                  ["latest.json"])
        self.prune()
        self.assertFalse(os.path.lexists(retained / "files" / "latest.json"))
        self.assertFalse(os.path.lexists(retained / "displaced.json"))
        self.assertEqual(b"operator\n", (retained / "files" / "draft.json").read_bytes())

    def test_a_file_open_elsewhere_is_kept_with_its_record(self):
        retained = self.retention("open", {"latest.json": b"{}\n"}, ["latest.json"])
        reader = os.open(retained / "files" / "latest.json", os.O_RDONLY)
        try:
            self.assertIn("latest.json is open in another process", self.prune())
        finally:
            os.close(reader)
        self.assertEqual(b"{}\n", (retained / "files" / "latest.json").read_bytes())
        self.assertTrue((retained / "displaced.json").exists())
        self.prune()
        self.assertFalse(os.path.lexists(retained))

    def test_unlink_if_empty_removes_only_an_empty_unopened_single_name(self):
        directory = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY)
        self.addCleanup(os.close, directory)
        (self.root / "empty").write_bytes(b"")
        self.assertEqual((True, None), mirror._unlink_if_empty(directory, "empty"))
        self.assertFalse(os.path.lexists(self.root / "empty"))
        (self.root / "full").write_bytes(b"x")
        self.assertEqual((False, None), mirror._unlink_if_empty(directory, "full"))
        (self.root / "linked").write_bytes(b"")
        os.link(self.root / "linked", self.root / "other-name")
        self.assertEqual((False, None), mirror._unlink_if_empty(directory, "linked"))
        (self.root / "held").write_bytes(b"")
        holder = os.open(self.root / "held", os.O_RDONLY)
        try:
            self.assertEqual((False, None), mirror._unlink_if_empty(directory, "held"))
        finally:
            os.close(holder)
        os.symlink(self.root / "full", self.root / "link")
        self.assertEqual((False, None), mirror._unlink_if_empty(directory, "link"))
        for name in ("full", "linked", "other-name", "held", "link"):
            self.assertTrue(os.path.lexists(self.root / name), name)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--real-finalized-provider":
        result = unittest.TextTestRunner(verbosity=2).run(real_finalized_suite(Path(sys.argv[2]).resolve()))
        raise SystemExit(not result.wasSuccessful())
    unittest.main()
