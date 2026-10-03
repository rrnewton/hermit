#!/usr/bin/env python3
"""Scenario tests for ci/buck-e2e/cell.sh and the container choice in ci/buck-e2e/defs.bzl.

CellTest runs cell.sh against a scratch bundle whose test-harness and
ci/hermetic/run-in-pinned-root.sh are stand-ins that record how they were called and
write the outputs a harness would. It checks that a cell marked
HERMIT_E2E_CONTAINER=pinned-root runs the harness only through the wrapper, with the
bundle at /src/bundle, outputs under the /results mount and a /test workdir, and that a
missing image, a non-local route or an unknown container value are reported as an
ERROR rather than run on the host. Nothing here needs podman, /test or capabilities.

ContainerChoiceTest evaluates defs.bzl's hermit_e2e_cells over the real
ci/expected-e2e-plan.json with stand-ins for the Buck builtins and checks which cells
get the pinned-root container in hybrid and local routing.
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


CI = Path(__file__).resolve().parent
CELL_SH = CI / "buck-e2e" / "cell.sh"
DEFS = CI / "buck-e2e" / "defs.bzl"
PLAN = CI / "expected-e2e-plan.json"
RE_EXCLUSIONS = CI / "buck-e2e" / "re_exclusions.json"
TEST, MODE, BACKEND = "c-programs/cpuid-probe", "verify", "dbt"
CELL = f"{TEST}/{MODE}@{BACKEND}"
SLUG = "c-programs-cpuid-probe-verify-dbt"

# test-harness, for `run ... --results F --junit F --tpx-json F`: records its argv and
# the environment cell.sh gives it, then writes one PASS row, its test_done and the
# verify evidence a passing verify cell must return. HARNESS_ROOT_MAP maps a container
# path prefix to the host directory behind it, the way the fake wrapper's mounts would.
FAKE_HARNESS = r"""#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
flag = lambda name: args[args.index(name) + 1]
mapping = json.loads(os.environ.get("HARNESS_ROOT_MAP") or "{}")
def host(path):
    for prefix, target in mapping.items():
        if path == prefix or path.startswith(prefix + "/"):
            return target + path[len(prefix):]
    return path
keys = ("HERMIT_E2E_EMPTY_WORKDIR", "E2E_RESULT_ROOT", "E2E_BUILD_ROOT", "VALIDATE_RUN_STATE",
        "E2E_RUN_ID", "HERMIT_BIN", "HERMIT_INSTALL_DIR", "E2E_KEEP_VERIFY_LOGS", "E2E_PARITY_POST_PASS")
with open(os.environ["FAKE_CALLS"], "a") as calls:
    calls.write(json.dumps({"who": "harness", "argv": args, "env": {k: os.environ.get(k) for k in keys}}) + "\n")
# Like hermit, which writes its private verify summary into its working directory (the
# repository root when no enclosing checkout ignores `ignored/`), and a write through an
# existing bundle file, which a hard-linked copy would carry back into the bundle.
repo = host(flag("--repo-root"))
with open(os.path.join(repo, ".hermit-verify-summary-fake"), "w") as f:
    f.write("summary\n")
if os.environ.get("HERMIT_BIN", "").startswith("/src/"):
    with open(host(os.environ["HERMIT_BIN"]), "a") as f:
        f.write("written by the cell\n")
outcome = os.environ.get("FAKE_OUTCOME", "PASS")
results, tpx = host(flag("--results")), host(flag("--tpx-json"))
os.makedirs(os.path.dirname(results), exist_ok=True)
test, mode, backend = flag("--test"), flag("--mode"), flag("--backend")
with open(results, "w") as f:
    f.write(json.dumps({"test": test, "mode": mode, "backend": backend, "outcome": outcome}) + "\n")
with open(host(flag("--junit")), "w") as f:
    f.write("<testsuite/>\n")
status = "passed" if outcome == "PASS" else "failed"
with open(tpx, "w") as f:
    f.write(json.dumps({"op": "test_done", "test": test + "/" + mode + "@" + backend, "status": status,
                        "details": json.dumps({"outcome": outcome})}) + "\n")
celldir = os.path.join(host(os.environ["E2E_RESULT_ROOT"]), "runs", os.environ["E2E_RUN_ID"],
                       os.environ["FAKE_SLUG"])
os.makedirs(os.path.join(celldir, "verify-logs"), exist_ok=True)
with open(os.path.join(celldir, "verify-1.json"), "w") as f:
    f.write("{}\n")
for n in (1, 2):
    with open(os.path.join(celldir, "verify-logs", "run%d_log_detlog" % n), "w") as f:
        f.write("detlog\n")
"""

# ci/hermetic/run-in-pinned-root.sh. `--check-image` exits FAKE_IMAGE_RC. A run records
# its options, the --env values it was asked to forward, and the mountpoints present in
# --src, then runs the command after `--` with /src, /results and /validate-run-state
# mapped to --src, $E2E_RESULT_ROOT and $VALIDATE_RUN_STATE, the way podman would.
FAKE_WRAPPER = r"""#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
calls = os.environ["FAKE_CALLS"]
if args[:1] == ["--check-image"]:
    with open(calls, "a") as f:
        f.write(json.dumps({"who": "check-image", "argv": args}) + "\n")
    rc = int(os.environ.get("FAKE_IMAGE_RC", "0"))
    if rc:
        print("run-in-pinned-root: image localhost/x@sha256:00 is not present", file=sys.stderr)
    sys.exit(rc)
split = args.index("--")
opts, command = args[:split], args[split + 1:]
src = opts[opts.index("--src") + 1]
forwarded = {opts[i + 1]: os.environ.get(opts[i + 1]) for i, o in enumerate(opts) if o == "--env"}
mountpoints = sorted(p for p in ("target", "agent-utils/rs/target", "agent-utils/rs/.agent-utils-locks",
                                 "agent-utils/rs/.agent-utils-snapshots") if os.path.isdir(os.path.join(src, p)))
with open(calls, "a") as f:
    f.write(json.dumps({"who": "wrapper", "opts": opts, "command": command, "forwarded": forwarded,
                        "mountpoints": mountpoints}) + "\n")
mapping = {"/src": src, "/results": os.environ["E2E_RESULT_ROOT"],
           "/validate-run-state": os.environ["VALIDATE_RUN_STATE"]}
env = {k: v for k, v in os.environ.items() if k not in forwarded}
env.update({"E2E_RESULT_ROOT": "/results", "VALIDATE_RUN_STATE": "/validate-run-state",
            "HARNESS_ROOT_MAP": json.dumps(mapping)})
env.update({k: v for k, v in forwarded.items() if k not in ("E2E_RESULT_ROOT", "VALIDATE_RUN_STATE")})
# The command is `env NAME=VALUE... timeout ... /src/bundle/bin/test-harness ...`: run it
# with the container paths of its executable mapped to the host.
command = [mapping["/src"] + c[len("/src"):] if c.startswith("/src/bundle/bin/") else c for c in command]
sys.exit(subprocess.run(command, env=env).returncode)
"""


def write_exe(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class CellTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-cell-test."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.bundle = self.tmp / "bundle"
        write_exe(self.bundle / "bin" / "test-harness", FAKE_HARNESS)
        write_exe(self.bundle / "src" / "ci" / "hermetic" / "run-in-pinned-root.sh", FAKE_WRAPPER)
        (self.bundle / "build").mkdir()
        (self.bundle / "hermit" / "install").mkdir(parents=True)
        (self.bundle / "hermit" / "hermit").write_text("hermit\n")
        (self.bundle / "run-state").mkdir()
        (self.bundle / "SOURCE_SHA").write_text("a" * 40 + "\n")
        self.before = sorted(str(p.relative_to(self.bundle)) for p in self.bundle.rglob("*"))
        self.calls = self.tmp / "calls.jsonl"
        self.calls.touch()

    def run_cell(self, **env: str) -> tuple[dict, dict | None]:
        artifacts = self.tmp / "tpx" / "artifacts"
        annotations = self.tmp / "tpx" / "annotations"
        shutil.rmtree(self.tmp / "tpx", ignore_errors=True)
        base = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.tmp),
            "TEST_RESULT_ARTIFACTS_DIR": str(artifacts),
            "TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR": str(annotations),
            "HERMIT_E2E_BUNDLE": str(self.bundle),
            "HERMIT_E2E_ROUTE": "local",
            "FAKE_CALLS": str(self.calls),
            "FAKE_SLUG": SLUG,
        }
        base.update(env)
        proc = subprocess.run(["bash", str(CELL_SH), TEST, MODE, BACKEND], env=base,
                              capture_output=True, text=True, timeout=120)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
        self.assertEqual(lines[-1], {"op": "all_done"}, proc.stdout)
        self.assertEqual(len(lines), 2, proc.stdout)
        result = artifacts / "result.json"
        return lines[0], (json.loads(result.read_text()) if result.exists() else None)

    def calls_by(self, who: str) -> list[dict]:
        return [c for c in map(json.loads, self.calls.read_text().splitlines()) if c["who"] == who]

    def assert_error(self, done: dict, *needles: str) -> None:
        self.assertEqual(done["status"], "failed")
        details = json.loads(done["details"])
        self.assertEqual(details["outcome"], "ERROR")
        for needle in needles:
            self.assertIn(needle, details["reason"])
        self.assertEqual(self.calls_by("harness"), [], "the harness must not run")

    def test_pinned_root_runs_the_harness_only_through_the_wrapper(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root",
                                     HERMIT_E2E_CONTAINER_REASON="dbt backend: needs CAP_SYS_ADMIN")
        self.assertEqual(done["status"], "passed", done)
        self.assertEqual(json.loads(done["details"])["outcome"], "PASS")
        self.assertEqual(len(self.calls_by("check-image")), 1)
        [wrapper] = self.calls_by("wrapper")
        opts, command = wrapper["opts"], wrapper["command"]
        self.assertIn("--src-rw", opts, "hermit writes its verify summary into /src/bundle/src")
        self.assertEqual(wrapper["mountpoints"], ["agent-utils/rs/.agent-utils-locks",
                                                  "agent-utils/rs/.agent-utils-snapshots",
                                                  "agent-utils/rs/target", "target"])
        self.assertEqual(wrapper["forwarded"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")
        for name in ("E2E_RESULT_ROOT", "VALIDATE_RUN_STATE", "E2E_RUN_ID"):
            self.assertTrue(wrapper["forwarded"].get(name), name)
        self.assertEqual(wrapper["forwarded"]["E2E_KEEP_VERIFY_LOGS"], "1")
        self.assertEqual(wrapper["forwarded"]["E2E_PARITY_POST_PASS"], "0")
        self.assertNotIn("E2E_BUILD_ROOT", wrapper["forwarded"],
                         "the wrapper would replace E2E_BUILD_ROOT with /src/target/e2e-build")
        self.assertEqual(command[0], "env")
        for assignment in ("E2E_BUILD_ROOT=/src/bundle/build", "HERMIT_BIN=/src/bundle/hermit/hermit",
                           "HERMIT_INSTALL_DIR=/src/bundle/hermit/install"):
            self.assertIn(assignment, command)
        self.assertIn("/src/bundle/bin/test-harness", command)
        flag = lambda name: command[command.index(name) + 1]
        self.assertEqual(flag("--repo-root"), "/src/bundle/src")
        self.assertEqual(flag("--results"), "/results/buck-cell-out/results.jsonl")
        self.assertEqual(flag("--tpx-json"), "/results/buck-cell-out/tpx.jsonl")
        self.assertEqual((flag("--test"), flag("--mode"), flag("--backend")), (TEST, MODE, BACKEND))
        self.assertTrue(all(not c.startswith(str(self.tmp)) for c in command),
                        "the in-container command must not name host paths")
        [harness] = self.calls_by("harness")
        self.assertEqual(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")
        self.assertEqual(harness["env"]["E2E_RESULT_ROOT"], "/results")
        self.assertEqual(harness["env"]["E2E_BUILD_ROOT"], "/src/bundle/build")
        self.assertEqual(result["container"], "pinned-root")
        self.assertEqual(result["container_reason"], "dbt backend: needs CAP_SYS_ADMIN")
        self.assertEqual(result["empty_workdir"], "HERMIT_E2E_EMPTY_WORKDIR=/test")
        self.assertTrue(result["evidence_complete"], result)
        self.assertEqual(result["outcome"], "PASS")
        after = sorted(str(p.relative_to(self.bundle)) for p in self.bundle.rglob("*"))
        self.assertEqual(after, self.before, "cell.sh must not write into the bundle")
        self.assertEqual((self.bundle / "hermit" / "hermit").read_text(), "hermit\n",
                         "a write in the container reached the bundle: the copy shares its inodes")
        self.assertEqual(list(self.tmp.joinpath("tpx").glob("hermit-cell.*")), [], "scratch left behind")

    def test_pinned_root_failure_is_reported(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", FAKE_OUTCOME="FAIL")
        self.assertEqual(done["status"], "failed")
        self.assertEqual(json.loads(done["details"])["outcome"], "FAIL")
        self.assertEqual(result["outcome"], "FAIL")

    def test_missing_image_is_an_error_not_a_host_run(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", FAKE_IMAGE_RC="2")
        self.assert_error(done, "pinned-root image unavailable", "is not present", "ci/hermetic/build-image.sh")
        self.assertEqual(self.calls_by("wrapper"), [])
        self.assertIsNone(result)

    def test_missing_wrapper_is_an_error(self) -> None:
        (self.bundle / "src" / "ci" / "hermetic" / "run-in-pinned-root.sh").unlink()
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root")
        self.assert_error(done, "run-in-pinned-root.sh")

    def test_pinned_root_on_re_is_an_error(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", HERMIT_E2E_ROUTE="re")
        self.assert_error(done, "needs a local route", "'re'")
        self.assertEqual(self.calls_by("check-image"), [])

    def test_unknown_container_is_an_error(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="docker")
        self.assert_error(done, "unknown HERMIT_E2E_CONTAINER 'docker'")

    def test_no_container_runs_the_harness_on_the_host(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_ROUTE="re")
        self.assertEqual(done["status"], "passed", done)
        self.assertEqual(self.calls_by("check-image") + self.calls_by("wrapper"), [])
        [harness] = self.calls_by("harness")
        self.assertIsNone(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "RE cells use the bound /tmp/test workdir")
        self.assertEqual(harness["env"]["E2E_BUILD_ROOT"], str(self.bundle / "build"))
        self.assertEqual(harness["env"]["HERMIT_BIN"], str(self.bundle / "hermit" / "hermit"))
        self.assertEqual(result["container"], "")
        self.assertEqual(result["empty_workdir"], "")


class _Anything:
    """Stand-in for Buck builtins defs.bzl names at load time but these tests never call."""

    def __getattr__(self, _name: str) -> "_Anything":
        return self

    def __call__(self, *_args, **_kwargs) -> "_Anything":
        return self


def evaluate_cells(routing: str) -> dict[str, dict]:
    """Runs defs.bzl's hermit_e2e_cells over the real plan; returns each target's attrs by name."""
    targets: dict[str, dict] = {}
    rule_calls = []

    def rule(impl, attrs):
        rule_calls.append(impl.__name__)
        if impl.__name__ == "_cell_test_impl":
            return lambda **kwargs: targets.__setitem__(kwargs["name"], kwargs)
        return _Anything()

    config = {("hermit_e2e", "routing"): routing}
    namespace = {
        "rule": rule,
        "attrs": _Anything(),
        "read_root_config": lambda section, key, default=None: config.get((section, key), default),
        "native": _Anything(),
    }
    exec(compile(DEFS.read_text(), str(DEFS), "exec"), namespace)
    assert "_cell_test_impl" in rule_calls, rule_calls
    namespace["hermit_e2e_cells"](json.loads(PLAN.read_text()), json.loads(RE_EXCLUSIONS.read_text()))
    return targets


class ContainerChoiceTest(unittest.TestCase):
    plan = json.loads(PLAN.read_text())["cells"]

    def slug(self, cell: dict) -> str:
        return "{}-{}-{}".format(cell["test"].replace("/", "-"), cell["mode"], cell["backend"])

    def check(self, routing: str) -> dict[str, dict]:
        targets = evaluate_cells(routing)
        self.assertEqual(len(targets), len(self.plan))
        for cell in self.plan:
            target = targets[self.slug(cell)]
            env, labels = target["env"], target["labels"]
            expected = target["route"] == "local" and (
                cell["lane"] == "privileged"
                or cell["backend"] == "dbt"
                or (cell["test"], cell["mode"]) == ("c-programs/environment-and-workdir", "custom"))
            self.assertEqual(env["HERMIT_E2E_CONTAINER"], "pinned-root" if expected else "", target["name"])
            self.assertEqual(bool(env["HERMIT_E2E_CONTAINER_REASON"]), expected, target["name"])
            self.assertEqual("hermit_e2e_container_pinned_root" in labels, expected, target["name"])
        return targets

    def containerized(self, targets: dict[str, dict]) -> set[str]:
        return {n for n, t in targets.items() if t["env"]["HERMIT_E2E_CONTAINER"] == "pinned-root"}

    def test_hybrid(self) -> None:
        targets = self.check("hybrid")
        chosen = self.containerized(targets)
        privileged = {self.slug(c) for c in self.plan if c["lane"] == "privileged"}
        self.assertTrue(privileged, "the plan has privileged-lane cells")
        self.assertLessEqual(privileged, chosen)
        self.assertIn("c-programs-cpuid-probe-verify-dbt", chosen)
        self.assertIn("c-programs-environment-and-workdir-custom-ptrace", chosen)
        # A portable DBT cell stays on RE, where the workdir is not requested.
        portable_dbt = {self.slug(c) for c in self.plan if c["lane"] == "portable" and c["backend"] == "dbt"}
        self.assertTrue(portable_dbt)
        for name in portable_dbt:
            self.assertEqual(targets[name]["route"], "re", name)
        self.assertEqual(chosen, privileged | {"c-programs-environment-and-workdir-custom-ptrace"})

    def test_local(self) -> None:
        chosen = self.containerized(self.check("local"))
        dbt = {self.slug(c) for c in self.plan if c["backend"] == "dbt"}
        self.assertLessEqual(dbt, chosen)


if __name__ == "__main__":
    unittest.main()
