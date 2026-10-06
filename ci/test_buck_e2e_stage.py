#!/usr/bin/env python3
"""Scenario tests for the host-prerequisite check in ci/buck-e2e/stage --from-cargo.

Each case runs a copy of stage in a scratch tree, with a PATH holding only the commands
it needs and a scratch library directory (HERMIT_STAGE_HOST_LIBDIR) in place of
/usr/lib64. Refuse: a missing patchelf, readelf, strip or cmake, or a missing libunwind
library, stops stage with the dnf package that provides it and the documentation
section, and the previous staging is left in place. Accept: with everything present,
stage gets past the check and removes the previous staging (it then fails on the
scratch tree, which is not a git checkout). --bundle needs none of them.

StageCargoOverlapTest runs stage in a committed scratch checkout with a fake cargo. The
release harness build and the validate-profile hermit build, in a target directory of
its own, overlap detcore-dbt and the install bundle and finish before the harness
binaries are stripped, each build runs main's exact command line, and the hermit binary
is copied into target/validate only after its build ends, even when it ends last; a
failed background build stops stage with its exit status and its log; a failed
detcore-dbt build stops stage and both running background builds with it, and each
background build's own child gets the SIGTERM too (cargo does not pass it on, so stage
signals the build's whole process group).
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


STAGE = Path(__file__).resolve().parent / "buck-e2e" / "stage"
TOOLS = ("patchelf", "readelf", "strip", "cmake")
LIBS = ("libunwind.so.8", "libunwind-x86_64.so.8")
# Commands stage runs on its way to the check and just after it.
HOST_COMMANDS = ("dirname", "realpath", "rm", "mkdir", "git", "cp", "python3", "du", "cut", "cat")
DOC = '(docs/BUCK2_OSS.md, "Host prerequisites for Buck validation")'


class StagePrerequisiteTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-stage-test."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.stage = self.tmp / "tree" / "ci" / "buck-e2e" / "stage"
        self.stage.parent.mkdir(parents=True)
        shutil.copy2(STAGE, self.stage)
        self.sentinel = self.stage.parent / "staged" / "SENTINEL"
        self.sentinel.parent.mkdir()
        self.sentinel.write_text("previous staging\n")
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name in HOST_COMMANDS:
            real = shutil.which(name)
            self.assertIsNotNone(real, name)
            (self.bin / name).symlink_to(real)
        for name in TOOLS:
            self.tool(name).write_text("#!/bin/sh\nexit 0\n")
            self.tool(name).chmod(0o755)
        self.libdir = self.tmp / "lib64"
        self.libdir.mkdir()
        for name in LIBS:
            (self.libdir / name).write_text(name + "\n")

    def tool(self, name: str) -> Path:
        return self.bin / name

    def run_stage(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        base = {"PATH": str(self.bin), "HOME": str(self.tmp), "HERMIT_STAGE_HOST_LIBDIR": str(self.libdir)}
        base.update(env)
        return subprocess.run(["/bin/bash", str(self.stage), *args], env=base, capture_output=True,
                              text=True, timeout=60)

    def assert_refused(self, proc: subprocess.CompletedProcess, what: str, package: str) -> None:
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn(f"ci/buck-e2e/stage: {what} is missing; install it with  sudo dnf install -y {package}",
                      proc.stderr)
        self.assertIn(DOC, proc.stderr)
        self.assertTrue(self.sentinel.exists(), "a refusal removed the previous staging")

    def test_each_missing_tool_names_its_package(self) -> None:
        for name, package in (("patchelf", "patchelf"), ("readelf", "binutils"), ("strip", "binutils"),
                              ("cmake", "cmake")):
            with self.subTest(name):
                saved = self.tool(name).read_text()
                self.tool(name).unlink()
                try:
                    self.assert_refused(self.run_stage("--from-cargo"), name, package)
                finally:
                    self.tool(name).write_text(saved)
                    self.tool(name).chmod(0o755)

    def test_cmake_named_by_the_cmake_variable(self) -> None:
        self.tool("cmake").rename(self.tool("cmake-pinned"))
        self.assert_refused(self.run_stage("--from-cargo"), "cmake", "cmake")
        proc = self.run_stage("--from-cargo", CMAKE="cmake-pinned")
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), proc.stderr)

    def test_each_missing_libunwind_library_names_libunwind_devel(self) -> None:
        for name in LIBS:
            with self.subTest(name):
                (self.libdir / name).unlink()
                try:
                    self.assert_refused(self.run_stage("--from-cargo"), f"{self.libdir}/{name}", "libunwind-devel")
                finally:
                    (self.libdir / name).write_text(name + "\n")

    def test_all_present_passes_the_check(self) -> None:
        proc = self.run_stage("--from-cargo")
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), f"stage stopped before replacing the staging: {proc.stderr}")
        self.assertNotEqual(proc.returncode, 0, "the scratch tree is not a checkout, so stage cannot finish")

    def test_bundle_mode_needs_no_host_packages(self) -> None:
        for name in TOOLS:
            self.tool(name).unlink()
        shutil.rmtree(self.libdir)
        proc = self.run_stage("--bundle", str(self.tmp / "no-bundle"))
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), proc.stderr)


FAKE_CARGO = r"""#!/usr/bin/env python3
import json, os, signal, subprocess, sys, time
args = sys.argv[1:]
if "--release" in args:
    label = "release"
else:
    label = "+".join(args[i + 1] for i, a in enumerate(args) if a == "-p")
target_dir = args[args.index("--target-dir") + 1]
# Like cargo, every build creates its profile directory.
os.makedirs(os.path.join(target_dir, "release" if "--release" in args else args[args.index("--profile") + 1]),
            exist_ok=True)
log = os.environ["FAKE_CARGO_LOG"]
def note(event):
    with open(log, "a") as f:
        f.write(f"{event} {label} {time.monotonic():.3f} {target_dir}\n")
# A child that records the SIGTERM it gets, so a test can tell a group-wide SIGTERM from
# a later SIGKILL. It reports "ready" only once its handler is installed: a SIGTERM that
# arrives before then kills it silently, which reads as no SIGTERM at all.
CHILD = '''import signal, sys, time
def term(*_):
    with open(sys.argv[1], "a") as f:
        f.write(f"child-term {sys.argv[2]} {time.monotonic():.3f} -\\n")
    sys.exit(143)
signal.signal(signal.SIGTERM, term)
print("ready", flush=True)
time.sleep(30)
'''
def killed(*_):
    note("killed")
    sys.exit(143)
# Like cargo, SIGTERM stops only this process: a child it started (rustc, a build
# script) keeps running unless its whole process group is signalled.
signal.signal(signal.SIGTERM, killed)
if os.environ.get("FAKE_CARGO_CHILDREN"):
    # The child is started, and its SIGTERM handler installed, before the build notes
    # "start", so a build that has started has a child able to record a SIGTERM.
    child = subprocess.Popen([sys.executable, "-c", CHILD, log, label], stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if child.stdout.readline() != b"ready\n":
        note("child-not-ready")
        sys.exit(98)
    with open(os.environ["FAKE_CARGO_CHILDREN"], "a") as f:
        f.write(f"{label} {child.pid}\n")
note("start")
with open(log, "a") as f:
    f.write(f"argv {label} {json.dumps(args)}\n")
print(f"fake cargo {label} output", file=sys.stderr)
if os.environ.get("FAKE_CARGO_FAIL") == label:
    # Fail only once every named build (comma-separated) has started, so the test sees
    # each one running.
    waited_for = [name for name in os.environ.get("FAKE_CARGO_FAIL_AFTER_START", "").split(",") if name]
    deadline = time.monotonic() + 20
    while not all(f"start {name} " in open(log).read() for name in waited_for):
        if time.monotonic() > deadline:
            note("never-started")
            sys.exit(99)
        time.sleep(0.05)
    note("fail")
    sys.exit(int(os.environ.get("FAKE_CARGO_FAIL_STATUS", "3")))
time.sleep(float(os.environ.get(f"FAKE_CARGO_SECONDS_{label.replace('-', '_').replace('+', '_')}", "0.5")))
if label == "hermit":
    with open(os.path.join(target_dir, "validate", "hermit"), "w") as f:
        f.write("fake hermit built in " + target_dir + "\n")
note("end")
"""


class StageCargoOverlapTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-stage-overlap."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        tree = self.tmp / "tree"
        self.stage = tree / "ci" / "buck-e2e" / "stage"
        self.stage.parent.mkdir(parents=True)
        shutil.copy2(STAGE, self.stage)
        (tree / ".gitignore").write_text("ci/buck-e2e/staged/\n")
        git = ["git", "-C", str(tree), "-c", "user.email=t@example.invalid", "-c", "user.name=t",
               "-c", "commit.gpgsign=false"]
        subprocess.run(git[:3] + ["init", "-q"], check=True)
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "fixture"], check=True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name in HOST_COMMANDS + ("mktemp", "setsid", "sleep"):
            real = shutil.which(name)
            self.assertIsNotNone(real, name)
            (self.bin / name).symlink_to(real)
        for name in TOOLS:
            (self.bin / name).write_text("#!/bin/sh\nexit 0\n")
            (self.bin / name).chmod(0o755)
        (self.bin / "strip").write_text('#!/bin/sh\necho "strip $*" >>"$FAKE_CARGO_LOG"\n')
        (self.bin / "install").write_text(f'#!/bin/sh\necho "install $*" >>"$FAKE_CARGO_LOG"\nexec {shutil.which("install")} "$@"\n')
        (self.bin / "cargo").write_text(FAKE_CARGO)
        for name in ("strip", "install", "cargo"):
            (self.bin / name).chmod(0o755)
        self.libdir = self.tmp / "lib64"
        self.libdir.mkdir()
        for name in LIBS:
            (self.libdir / name).write_text(name + "\n")
        self.log = self.tmp / "cargo.log"

    def run_stage(self, **env: str) -> subprocess.CompletedProcess:
        base = {"PATH": str(self.bin), "HOME": str(self.tmp), "HERMIT_STAGE_HOST_LIBDIR": str(self.libdir),
                "FAKE_CARGO_LOG": str(self.log)}
        base.update(env)
        return subprocess.run(["/bin/bash", str(self.stage), "--from-cargo"], env=base, capture_output=True,
                              text=True, timeout=60)

    def events(self) -> list[tuple[str, str, float]]:
        rows = []
        self.target_dirs = {}
        self.argv = {}
        for line in self.log.read_text().splitlines():
            parts = line.split()
            if parts[0] in ("strip", "install"):
                rows.append((parts[0], parts[-1], 0.0))
            elif parts[0] == "argv":
                self.argv[parts[1]] = json.loads(line.split(" ", 2)[2])
            else:
                rows.append((parts[0], parts[1], float(parts[2])))
                self.target_dirs[parts[1]] = parts[3]
        return rows

    def test_the_background_builds_overlap_the_validate_builds_and_finish_before_strip(self) -> None:
        # Each background build outlasts detcore-dbt and the install bundle (0.5 s each),
        # so strip waits for both.
        proc = self.run_stage(FAKE_CARGO_SECONDS_release="2.5", FAKE_CARGO_SECONDS_hermit="2.0")
        events = self.events()
        names = [(event, label) for event, label, _ in events]
        times = {(event, label): t for event, label, t in events if event != "strip"}
        first_validate = "detcore-dbt"
        strips = [i for i, (event, _) in enumerate(names) if event == "strip"]
        self.assertTrue(strips, f"stage never reached strip: {proc.stderr}")
        for background in ("release", "hermit"):
            self.assertLess(times[("start", background)], times[("end", first_validate)], events)
            self.assertGreater(times[("end", background)], times[("start", first_validate)], events)
            self.assertLess(names.index(("end", background)), strips[0], events)
            self.assertIn(f"fake cargo {background} output", proc.stderr)
        target = str(self.stage.parent.parent.parent / "target")
        self.assertEqual(self.target_dirs["release"], target)
        self.assertEqual(self.target_dirs["detcore-dbt"], target)
        self.assertEqual(self.target_dirs["hermit-install+detcore-sabre+detcore-liteinst"], target)
        self.assertEqual(self.target_dirs["hermit"], target + "/stage-hermit")
        self.assertEqual((Path(target) / "validate" / "hermit").read_text(),
                         f"fake hermit built in {target}/stage-hermit\n")
        self.assertLess(names.index(("end", "hermit")), names.index(("install", f"{target}/validate/hermit")), events)
        # Each build is main's command; only the hermit build's target directory moved.
        manifest = str(self.stage.parent.parent.parent / "Cargo.toml")
        common = ["build", "--manifest-path", manifest, "--locked"]
        self.assertEqual(self.argv, {
            "release": common + ["--release", "-p", "hermit-manifest-plan", "--bins", "--target-dir", target],
            "hermit": common + ["--profile", "validate", "-p", "hermit", "--bin", "hermit",
                                "--features", "hermit/third-party-backends", "--target-dir", target + "/stage-hermit"],
            "detcore-dbt": common + ["--profile", "validate", "-p", "detcore-dbt", "--target-dir", target],
            "hermit-install+detcore-sabre+detcore-liteinst": common + [
                "--profile", "validate", "-p", "hermit-install", "-p", "detcore-sabre", "-p", "detcore-liteinst",
                "--target-dir", target],
        })

    def test_the_hermit_binary_is_copied_only_after_its_build_finishes_last(self) -> None:
        # In practice the hermit build (about 120 s) finishes long after the release build
        # (about 55 s); the copy must wait for it, not for the release build.
        proc = self.run_stage(FAKE_CARGO_SECONDS_release="0.5", FAKE_CARGO_SECONDS_hermit="3.0")
        names = [(event, label) for event, label, _ in self.events()]
        target = str(self.stage.parent.parent.parent / "target")
        copy = ("install", f"{target}/validate/hermit")
        self.assertIn(copy, names, proc.stderr)
        self.assertLess(names.index(("end", "release")), names.index(("end", "hermit")), names)
        self.assertLess(names.index(("end", "hermit")), names.index(copy), names)
        self.assertEqual((Path(target) / "validate" / "hermit").read_text(),
                         f"fake hermit built in {target}/stage-hermit\n")

    def test_a_failed_release_build_stops_stage_with_its_status_and_log(self) -> None:
        proc = self.run_stage(FAKE_CARGO_FAIL="release", FAKE_CARGO_FAIL_STATUS="7")
        self.assertEqual(proc.returncode, 7, proc.stderr)
        self.assertIn("the release build of hermit-manifest-plan failed (exit 7)", proc.stderr)
        self.assertIn("fake cargo release output", proc.stderr)
        self.assertNotIn("strip", [event for event, _, _ in self.events()])

    def test_a_failed_hermit_build_stops_stage_with_its_status_and_log(self) -> None:
        proc = self.run_stage(FAKE_CARGO_FAIL="hermit", FAKE_CARGO_FAIL_STATUS="5")
        self.assertEqual(proc.returncode, 5, proc.stderr)
        self.assertIn("the validate-profile build of hermit failed (exit 5)", proc.stderr)
        self.assertIn("fake cargo hermit output", proc.stderr)
        self.assertNotIn("strip", [event for event, _, _ in self.events()])

    def test_a_failed_validate_build_stops_both_running_background_builds(self) -> None:
        children = self.tmp / "children"
        proc = self.run_stage(FAKE_CARGO_FAIL="detcore-dbt", FAKE_CARGO_FAIL_AFTER_START="hermit,release",
                              FAKE_CARGO_SECONDS_release="30", FAKE_CARGO_SECONDS_hermit="30",
                              FAKE_CARGO_CHILDREN=str(children))
        self.assertEqual(proc.returncode, 3, proc.stderr)
        names = [(event, label) for event, label, _ in self.events()]
        for background in ("release", "hermit"):
            self.assertIn(("killed", background), names)
            self.assertNotIn(("end", background), names)
        # The background builds' own children (rustc, build scripts) are stopped too,
        # before stage returns. (The failed foreground build's child is the fake's own
        # leftover; real cargo waits for its children before it exits.)
        pids = dict(line.split() for line in children.read_text().splitlines())
        for pid in pids.values():
            self.addCleanup(self.kill_quietly, int(pid))
        self.assertEqual(sorted(pids), ["detcore-dbt", "hermit", "release"])
        for background in ("release", "hermit"):
            self.assertFalse(self.running(int(pids[background])),
                             f"the {background} build's child outlived stage")
            self.assertIn(("child-term", background), names, "the child was not sent SIGTERM with its build")

    @staticmethod
    def running(pid: int) -> bool:
        try:
            return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
        except FileNotFoundError:
            return False

    @staticmethod
    def kill_quietly(pid: int) -> None:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass


if __name__ == "__main__":
    unittest.main()
